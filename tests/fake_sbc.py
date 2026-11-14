"""A stand-in for libsbc that counts codec states, so tests can prove none is leaked."""


class FakeLibSbc:
    def __init__(self) -> None:
        self.inits = 0
        self.finishes = 0
        self.live: set[int] = set()
        self.double_finishes = 0
        self.fail_init_numbers: set[int] = set()
        """1-based init calls that fail, e.g. {2} fails the decoder of a new link."""
        self.fail_encode = False

    def sbc_init_msbc(self, ref, _flags) -> int:
        self.inits += 1
        if self.inits in self.fail_init_numbers:
            return -1
        ref._obj.flags = self.inits  # a handle that survives until sbc_finish
        self.live.add(self.inits)
        return 0

    def sbc_finish(self, ref) -> None:
        self.finishes += 1
        handle = ref._obj.flags
        if handle not in self.live:
            self.double_finishes += 1
        self.live.discard(handle)

    def sbc_encode(self, ref, _src, src_len, _dst, _dst_len, written) -> int:
        assert ref._obj.flags in self.live, "encode on a released codec"
        if self.fail_encode:
            return -1
        written._obj.value = 57
        return src_len

    def sbc_decode(self, ref, _src, src_len, _dst, dst_len, written) -> int:
        assert ref._obj.flags in self.live, "decode on a released codec"
        written._obj.value = dst_len
        return src_len


def install(monkeypatch) -> FakeLibSbc:
    from voice_clone.hfp import msbc

    lib = FakeLibSbc()
    monkeypatch.setattr(msbc, "libsbc", lambda: lib)
    return lib
