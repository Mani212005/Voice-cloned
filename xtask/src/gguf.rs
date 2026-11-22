//! A minimal GGUF v3 splicer: rebuild a model file with some tensors taken from another
//! quantization of the same model.
//!
//! The output keeps the base file's header, metadata (copied byte for byte), tensor order
//! and alignment. Tensors whose name starts with the prefix take the donor's type and data;
//! every other tensor keeps the base's bytes. Offsets are recomputed and every tensor's data
//! starts on the alignment boundary, padded with zeros, so the output is a pure function of
//! the two inputs and the prefix.

use std::io::{Read, Write};

use anyhow::{Context, bail, ensure};

const MAGIC: &[u8; 4] = b"GGUF";
const DEFAULT_ALIGNMENT: u64 = 32;

/// One tensor's description in the tensor-info table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorInfo {
    pub name: String,
    pub dims: Vec<u64>,
    pub ggml_type: u32,
    /// Byte offset from the start of the data section.
    pub offset: u64,
}

/// A parsed GGUF file: the raw metadata section and the tensor table, plus the data.
#[derive(Debug)]
pub struct Gguf {
    pub version: u32,
    pub kv_count: u64,
    /// The key-value section exactly as it appears in the file.
    pub kv_raw: Vec<u8>,
    pub alignment: u64,
    pub tensors: Vec<TensorInfo>,
    /// Everything from the (aligned) start of the data section to the end of the file.
    pub data: Vec<u8>,
}

impl Gguf {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut r = Reader { b: bytes, pos: 0 };
        ensure!(r.take(4)? == MAGIC, "not a GGUF file");
        let version = r.u32()?;
        ensure!(version == 3, "GGUF version {version} is not supported");
        let tensor_count = r.u64()?;
        let kv_count = r.u64()?;
        let kv_start = r.pos;
        let mut alignment = DEFAULT_ALIGNMENT;
        for _ in 0..kv_count {
            let key = r.string()?;
            let ty = r.u32()?;
            if key == "general.alignment" {
                ensure!(ty == 4, "general.alignment must be a u32");
                alignment = u64::from(r.u32()?);
                ensure!(
                    alignment > 0 && alignment.is_power_of_two(),
                    "bad alignment"
                );
            } else {
                r.skip_value(ty)?;
            }
        }
        let kv_raw = bytes[kv_start..r.pos].to_vec();
        let mut tensors = Vec::with_capacity(usize::try_from(tensor_count)?);
        for _ in 0..tensor_count {
            let name = r.string()?;
            let n_dims = r.u32()?;
            ensure!(n_dims <= 8, "tensor {name} has {n_dims} dimensions");
            let dims = (0..n_dims)
                .map(|_| r.u64())
                .collect::<anyhow::Result<_>>()?;
            let ggml_type = r.u32()?;
            let offset = r.u64()?;
            tensors.push(TensorInfo {
                name,
                dims,
                ggml_type,
                offset,
            });
        }
        let data_start = align(r.pos as u64, alignment);
        let data_start = usize::try_from(data_start)?;
        ensure!(
            data_start <= bytes.len(),
            "truncated before the data section"
        );
        let g = Gguf {
            version,
            kv_count,
            kv_raw,
            alignment,
            tensors,
            data: bytes[data_start..].to_vec(),
        };
        for t in &g.tensors {
            g.tensor_data(t)?;
        }
        Ok(g)
    }

    /// The bytes of one tensor.
    pub fn tensor_data(&self, t: &TensorInfo) -> anyhow::Result<&[u8]> {
        let size = tensor_bytes(t)?;
        let start = usize::try_from(t.offset)?;
        let end = start
            .checked_add(usize::try_from(size)?)
            .context("tensor size overflows")?;
        self.data
            .get(start..end)
            .with_context(|| format!("tensor {} runs past the end of the file", t.name))
    }

    /// Serialize with `self.tensors` laid out in order, each on the alignment boundary.
    /// `data_of` gives each tensor's bytes.
    fn write_with<'a>(
        &self,
        tensors: &[TensorInfo],
        data_of: impl Fn(usize) -> anyhow::Result<&'a [u8]>,
        out: &mut impl Write,
    ) -> anyhow::Result<()> {
        let mut laid = Vec::with_capacity(tensors.len());
        let mut offset = 0u64;
        for (i, t) in tensors.iter().enumerate() {
            let len = data_of(i)?.len() as u64;
            laid.push(TensorInfo {
                offset,
                ..t.clone()
            });
            offset = align(offset + len, self.alignment);
        }
        let mut head = Vec::new();
        head.extend_from_slice(MAGIC);
        head.extend_from_slice(&self.version.to_le_bytes());
        head.extend_from_slice(&(laid.len() as u64).to_le_bytes());
        head.extend_from_slice(&self.kv_count.to_le_bytes());
        head.extend_from_slice(&self.kv_raw);
        for t in &laid {
            head.extend_from_slice(&(t.name.len() as u64).to_le_bytes());
            head.extend_from_slice(t.name.as_bytes());
            head.extend_from_slice(&(t.dims.len() as u32).to_le_bytes());
            for d in &t.dims {
                head.extend_from_slice(&d.to_le_bytes());
            }
            head.extend_from_slice(&t.ggml_type.to_le_bytes());
            head.extend_from_slice(&t.offset.to_le_bytes());
        }
        let padded = usize::try_from(align(head.len() as u64, self.alignment))?;
        head.resize(padded, 0);
        out.write_all(&head)?;
        let mut written = 0u64;
        for (i, t) in laid.iter().enumerate() {
            let pad = usize::try_from(t.offset - written)?;
            out.write_all(&vec![0u8; pad])?;
            let d = data_of(i)?;
            out.write_all(d)?;
            written = t.offset + d.len() as u64;
        }
        let tail = usize::try_from(align(written, self.alignment) - written)?;
        out.write_all(&vec![0u8; tail])?;
        Ok(())
    }
}

/// Write `base` with every tensor whose name starts with `prefix` replaced by the same-named
/// tensor of `donor` (its type and data). Returns how many tensors were replaced.
pub fn splice(
    base: &Gguf,
    donor: &Gguf,
    prefix: &str,
    out: &mut impl Write,
) -> anyhow::Result<usize> {
    let mut picked: Vec<(TensorInfo, &Gguf, TensorInfo)> = Vec::with_capacity(base.tensors.len());
    let mut replaced = 0;
    for t in &base.tensors {
        if t.name.starts_with(prefix) {
            let Some(d) = donor.tensors.iter().find(|d| d.name == t.name) else {
                bail!("donor has no tensor {}", t.name);
            };
            ensure!(
                d.dims == t.dims,
                "tensor {} has shape {:?} in the base and {:?} in the donor",
                t.name,
                t.dims,
                d.dims
            );
            picked.push((
                TensorInfo {
                    ggml_type: d.ggml_type,
                    ..t.clone()
                },
                donor,
                d.clone(),
            ));
            replaced += 1;
        } else {
            picked.push((t.clone(), base, t.clone()));
        }
    }
    let tensors: Vec<TensorInfo> = picked.iter().map(|(t, _, _)| t.clone()).collect();
    base.write_with(
        &tensors,
        |i| {
            let (_, src, info) = &picked[i];
            src.tensor_data(info)
        },
        out,
    )?;
    Ok(replaced)
}

/// Read a whole GGUF file.
pub fn read(path: &std::path::Path) -> anyhow::Result<Gguf> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .read_to_end(&mut bytes)?;
    Gguf::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn align(x: u64, a: u64) -> u64 {
    x.div_ceil(a) * a
}

/// (elements per block, bytes per block) of the ggml types these model files use.
fn block_layout(ggml_type: u32) -> Option<(u64, u64)> {
    Some(match ggml_type {
        0 => (1, 4),      // F32
        1 => (1, 2),      // F16
        2 => (32, 18),    // Q4_0
        3 => (32, 20),    // Q4_1
        6 => (32, 22),    // Q5_0
        7 => (32, 24),    // Q5_1
        8 => (32, 34),    // Q8_0
        10 => (256, 84),  // Q2_K
        11 => (256, 110), // Q3_K
        12 => (256, 144), // Q4_K
        13 => (256, 176), // Q5_K
        14 => (256, 210), // Q6_K
        15 => (256, 292), // Q8_K
        30 => (1, 2),     // BF16
        _ => return None,
    })
}

fn tensor_bytes(t: &TensorInfo) -> anyhow::Result<u64> {
    let Some((block, bytes)) = block_layout(t.ggml_type) else {
        bail!(
            "tensor {} has unsupported ggml type {}",
            t.name,
            t.ggml_type
        );
    };
    let n: u64 = t.dims.iter().product();
    ensure!(
        n.is_multiple_of(block),
        "tensor {} has {n} elements, not a multiple of {block}",
        t.name
    );
    Ok(n / block * bytes)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("length overflows")?;
        let s = self.b.get(self.pos..end).context("truncated GGUF file")?;
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn string(&mut self) -> anyhow::Result<String> {
        let n = usize::try_from(self.u64()?)?;
        Ok(String::from_utf8(self.take(n)?.to_vec())?)
    }

    fn skip_value(&mut self, ty: u32) -> anyhow::Result<()> {
        match ty {
            0 | 1 | 7 => self.take(1).map(|_| ()),
            2 | 3 => self.take(2).map(|_| ()),
            4..=6 => self.take(4).map(|_| ()),
            10..=12 => self.take(8).map(|_| ()),
            8 => self.string().map(|_| ()),
            9 => {
                let inner = self.u32()?;
                let n = self.u64()?;
                for _ in 0..n {
                    self.skip_value(inner)?;
                }
                Ok(())
            }
            _ => bail!("unknown GGUF value type {ty}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A small GGUF file: a string, an array and the alignment as metadata, and F32 / F16 /
    /// Q8_0 tensors with recognizable bytes.
    fn file(tensors: &[(&str, &[u64], u32, u8)], alignment: u32) -> Vec<u8> {
        let mut kv = Vec::new();
        let put_str = |v: &mut Vec<u8>, s: &str| {
            v.extend_from_slice(&(s.len() as u64).to_le_bytes());
            v.extend_from_slice(s.as_bytes());
        };
        put_str(&mut kv, "general.architecture");
        kv.extend_from_slice(&8u32.to_le_bytes());
        put_str(&mut kv, "chatterbox");
        put_str(&mut kv, "x.list");
        kv.extend_from_slice(&9u32.to_le_bytes());
        kv.extend_from_slice(&5u32.to_le_bytes());
        kv.extend_from_slice(&3u64.to_le_bytes());
        kv.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        put_str(&mut kv, "general.alignment");
        kv.extend_from_slice(&4u32.to_le_bytes());
        kv.extend_from_slice(&alignment.to_le_bytes());
        let g = Gguf {
            version: 3,
            kv_count: 3,
            kv_raw: kv,
            alignment: u64::from(alignment),
            tensors: vec![],
            data: vec![],
        };
        let infos: Vec<TensorInfo> = tensors
            .iter()
            .map(|(n, d, ty, _)| TensorInfo {
                name: (*n).to_owned(),
                dims: d.to_vec(),
                ggml_type: *ty,
                offset: 0,
            })
            .collect();
        let datas: Vec<Vec<u8>> = tensors
            .iter()
            .zip(&infos)
            .map(|((_, _, _, fill), t)| {
                vec![*fill; usize::try_from(tensor_bytes(t).unwrap()).unwrap()]
            })
            .collect();
        let mut out = Vec::new();
        g.write_with(&infos, |i| Ok(&datas[i]), &mut out).unwrap();
        out
    }

    #[test]
    fn replaces_only_prefixed_tensors_with_the_donor_type_and_bytes() {
        let base = file(
            &[
                ("s3.v.w", &[64], 1, 0xA1),
                ("s3.fd.a", &[256, 2], 12, 0xA2),
                ("s3.fd.b", &[32], 2, 0xA3),
                ("s3.tok.c", &[256], 12, 0xA4),
            ],
            32,
        );
        let donor = file(
            &[
                ("s3.tok.c", &[256], 8, 0xB4),
                ("s3.fd.b", &[32], 8, 0xB3),
                ("s3.fd.a", &[256, 2], 8, 0xB2),
                ("s3.v.w", &[64], 1, 0xB1),
            ],
            32,
        );
        let (b, d) = (Gguf::parse(&base).unwrap(), Gguf::parse(&donor).unwrap());
        let mut out = Vec::new();
        assert_eq!(splice(&b, &d, "s3.fd.", &mut out).unwrap(), 2);
        let o = Gguf::parse(&out).unwrap();
        assert_eq!(o.kv_raw, b.kv_raw, "metadata is copied verbatim");
        let names: Vec<&str> = o.tensors.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["s3.v.w", "s3.fd.a", "s3.fd.b", "s3.tok.c"]);
        let types: Vec<u32> = o.tensors.iter().map(|t| t.ggml_type).collect();
        assert_eq!(types, [1, 8, 8, 12]);
        let fills: Vec<u8> = o
            .tensors
            .iter()
            .map(|t| {
                let d = o.tensor_data(t).unwrap();
                assert!(d.iter().all(|&x| x == d[0]));
                d[0]
            })
            .collect();
        assert_eq!(fills, [0xA1, 0xB2, 0xB3, 0xA4]);
        assert!(o.tensors.iter().all(|t| t.offset % 32 == 0));
    }

    #[test]
    fn a_splice_that_matches_nothing_reproduces_the_base_exactly() {
        let base = file(
            &[("a", &[3], 0, 1), ("b", &[32], 8, 2), ("c", &[5], 1, 3)],
            64,
        );
        let b = Gguf::parse(&base).unwrap();
        let mut out = Vec::new();
        assert_eq!(splice(&b, &b, "nothing.", &mut out).unwrap(), 0);
        assert_eq!(out, base);
    }

    #[test]
    fn the_output_is_deterministic() {
        let base = file(&[("s3.fd.a", &[256], 12, 1), ("z", &[4], 0, 2)], 32);
        let donor = file(&[("s3.fd.a", &[256], 8, 9)], 32);
        let (b, d) = (Gguf::parse(&base).unwrap(), Gguf::parse(&donor).unwrap());
        let (mut x, mut y) = (Vec::new(), Vec::new());
        splice(&b, &d, "s3.fd.", &mut x).unwrap();
        splice(&b, &d, "s3.fd.", &mut y).unwrap();
        assert_eq!(x, y);
    }

    #[test]
    fn refuses_a_missing_or_reshaped_donor_tensor_and_bad_files() {
        let base = file(&[("s3.fd.a", &[256], 12, 1)], 32);
        let b = Gguf::parse(&base).unwrap();
        let other = Gguf::parse(&file(&[("s3.fd.x", &[256], 8, 1)], 32)).unwrap();
        assert!(splice(&b, &other, "s3.fd.", &mut Vec::new()).is_err());
        let reshaped = Gguf::parse(&file(&[("s3.fd.a", &[512], 8, 1)], 32)).unwrap();
        assert!(splice(&b, &reshaped, "s3.fd.", &mut Vec::new()).is_err());
        assert!(Gguf::parse(b"GGML....").is_err());
        // Cut into the tensor data, not just the alignment padding after it.
        assert!(Gguf::parse(&base[..base.len() - 100]).is_err());
    }
}
