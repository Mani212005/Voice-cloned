//! The HTTP API end to end in process: real router, real SQLite in a tempdir, real
//! scheduler, fake backend. Waits are on scheduler events, never on time.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use vc_api::{ApiConfig, AppState, router};
use vc_audio::MonoPcm;
use vc_backend::{Backend, FakeBackend, FakeControl};
use vc_core::{JobId, JobState, ManualClock};
use vc_engine::{Engine, EngineConfig, EngineContext, JobEvent};
use vc_store::{DataDir, Store};

struct Api {
    _dir: tempfile::TempDir,
    app: axum::Router,
    engine: Engine,
    data: DataDir,
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn code(&self) -> String {
        assert_eq!(
            self.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        self.json()["code"].as_str().unwrap().to_owned()
    }
}

impl Api {
    fn new(backend: Arc<dyn Backend>, engine_config: EngineConfig, api: ApiConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::open(&dir.path().join("data")).unwrap();
        let store = Arc::new(Store::open(&data.db_path()).unwrap());
        let ctx = EngineContext {
            store,
            data: data.clone(),
            backend,
            clock: Arc::new(ManualClock::new(1_763_200_000_000)),
            config: engine_config,
        };
        let engine = Engine::start(ctx).unwrap();
        let app = router(AppState {
            engine: engine.clone(),
            config: api,
            app_version: "test",
        });
        Self {
            _dir: dir,
            app,
            engine,
            data,
        }
    }

    fn fake() -> Self {
        Self::new(
            Arc::new(FakeBackend::new()),
            EngineConfig::default(),
            ApiConfig::default(),
        )
    }

    fn gated(engine_config: EngineConfig) -> (Self, FakeControl) {
        let (b, ctl) = FakeBackend::new().gated();
        (
            Self::new(Arc::new(b), engine_config, ApiConfig::default()),
            ctl,
        )
    }

    async fn call(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, uri: &str) -> Resp {
        self.call(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    async fn delete(&self, uri: &str) -> Resp {
        self.call(
            Request::builder()
                .method(Method::DELETE)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post_json(&self, uri: &str, body: Value) -> Resp {
        self.call(
            Request::post(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn post_multipart(&self, uri: &str, fields: &[(&str, Vec<u8>)]) -> Resp {
        let boundary = "vcd-test-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            if *name == "file" {
                body.extend_from_slice(
                    b"Content-Disposition: form-data; name=\"file\"; filename=\"ref.wav\"\r\n\
                      Content-Type: audio/wav\r\n\r\n",
                );
            } else {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                );
            }
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        self.call(
            Request::post(uri)
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    async fn consent(&self) -> String {
        let r = self
            .post_json(
                "/v1/consents",
                json!({
                    "speaker_name": "Owner",
                    "relationship": "self",
                    "attestation": "This is my own voice and I consent to cloning it.",
                    "languages": ["en", "hi"],
                }),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
        r.json()["id"].as_str().unwrap().to_owned()
    }

    async fn voice(&self) -> String {
        let consent = self.consent().await;
        let r = self
            .post_multipart(
                "/v1/voices",
                &[
                    ("consent_id", consent.into_bytes()),
                    ("name", b"me".to_vec()),
                    ("reference_language", b"en".to_vec()),
                    ("file", wav(16_000, 8.0)),
                ],
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&r.body)
        );
        r.json()["id"].as_str().unwrap().to_owned()
    }

    async fn job(&self, voice: &str, language: &str, text: &str) -> Resp {
        self.post_json(
            "/v1/jobs",
            json!({ "voice_id": voice, "language": language, "text": text }),
        )
        .await
    }

    async fn wait_finished(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<JobEvent>,
        job: &str,
    ) {
        let id: JobId = job.parse().unwrap();
        loop {
            if let JobEvent::Finished { job: j, .. } = events.recv().await.unwrap()
                && j == id
            {
                return;
            }
        }
    }
}

/// A synthetic reference: a quiet tone, nothing anyone said.
fn wav(rate: u32, seconds: f64) -> Vec<u8> {
    let n = (f64::from(rate) * seconds) as usize;
    let samples = (0..n)
        .map(|i| (6_000.0 * (i as f64 * 0.05).sin()) as i16)
        .collect();
    vc_audio::encode_wav_bytes(&MonoPcm {
        sample_rate: rate,
        samples,
    })
    .unwrap()
}

#[tokio::test]
async fn health_version_and_models() {
    let api = Api::fake();
    let r = api.get("/v1/health").await;
    assert_eq!(
        (r.status, r.json()),
        (StatusCode::OK, json!({"status": "ok"}))
    );
    let v = api.get("/v1/version").await.json();
    assert_eq!(v["api"], "v1");
    assert_eq!(v["backend"]["backend_id"], "fake");
    assert_eq!(v["consent_chain_head"], vc_store::GENESIS_HASH);
    let m = api.get("/v1/models").await.json();
    assert_eq!(m["models"][0]["capabilities"]["max_chunk_words"], 19);
    assert_eq!(m["models"][0]["capabilities"]["concurrency"], 1);
    assert_eq!(m["models"][0]["output_formats"], json!(["wav"]));
    let r = api.get("/v1/nope").await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::NOT_FOUND, "not_found".into())
    );
}

#[tokio::test]
async fn only_loopback_hosts_are_served() {
    let api = Api::fake();
    let with_host = |host: &str| {
        Request::get("/v1/health")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    };
    for host in [
        "127.0.0.1",
        "127.0.0.1:8740",
        "localhost",
        "LocalHost:8740",
        "[::1]",
        "[::1]:8740",
    ] {
        assert_eq!(
            api.call(with_host(host)).await.status,
            StatusCode::OK,
            "{host}"
        );
    }
    for host in [
        "evil.example",
        "evil.example:8740",
        "127.0.0.1.evil.example",
        "localhost.evil.example:8740",
        "10.0.0.5:8740",
        "[::2]",
        "127.0.0.1:",
        "127.0.0.1:80x",
        "[::1",
        "",
    ] {
        let r = api.call(with_host(host)).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "invalid_request".into()),
            "{host}"
        );
    }
    let r = api
        .call(
            Request::get("/v1/nope")
                .header(header::HOST, "evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn consents_are_own_voice_only_and_chain() {
    let api = Api::fake();
    let own = json!({
        "speaker_name": "Owner", "relationship": "self",
        "attestation": "This is my own voice.", "languages": ["en"],
    });
    let first = api.post_json("/v1/consents", own.clone()).await.json();
    assert_eq!(first["seq"], 1);
    assert_eq!(first["prev_hash"], vc_store::GENESIS_HASH);
    let head = api.get("/v1/version").await.json()["consent_chain_head"].clone();
    assert_eq!(head, first["record_hash"]);
    let second = api.post_json("/v1/consents", own).await.json();
    assert_eq!(second["prev_hash"], first["record_hash"]);

    let r = api
        .post_json(
            "/v1/consents",
            json!({"speaker_name": "Friend", "relationship": "friend",
                   "attestation": "ok", "languages": ["en"]}),
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "invalid_request".into())
    );
    let r = api
        .post_json("/v1/consents", json!({"speaker_name": "x"}))
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "invalid_request".into())
    );
    let id = first["id"].as_str().unwrap();
    assert_eq!(
        api.get(&format!("/v1/consents/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn enrollment_needs_consent_and_a_valid_reference() {
    let api = Api::fake();
    let r = api
        .post_multipart("/v1/voices", &[("file", wav(16_000, 8.0))])
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "consent_required".into())
    );
    let fake_consent = "cns_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let r = api
        .post_multipart(
            "/v1/voices",
            &[
                ("consent_id", fake_consent.into()),
                ("file", wav(16_000, 8.0)),
            ],
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::BAD_REQUEST, "consent_required".into())
    );

    let consent = api.consent().await;
    for (file, why) in [
        (b"not audio".to_vec(), "garbage"),
        (wav(16_000, 2.0), "too short"),
        (wav(16_000, 40.0), "too long"),
    ] {
        let r = api
            .post_multipart(
                "/v1/voices",
                &[("consent_id", consent.clone().into_bytes()), ("file", file)],
            )
            .await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::UNPROCESSABLE_ENTITY, "reference_invalid".into()),
            "{why}"
        );
    }
    let r = api
        .post_multipart(
            "/v1/voices",
            &[
                ("consent_id", consent.clone().into_bytes()),
                ("file", wav(16_000, 8.0)),
                ("file", wav(16_000, 8.0)),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        api.get("/v1/voices").await.json()["voices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn enroll_list_get_and_delete_a_voice() {
    let api = Api::fake();
    let id = api.voice().await;
    let v = api.get(&format!("/v1/voices/{id}")).await.json();
    assert_eq!(v["reference_language"], "en");
    assert_eq!(v["reference_seconds"], 8.0);
    assert_eq!(v["reference_sample_rate_hz"], 16_000);
    let path = api.data.reference_path(id.parse().unwrap());
    let stored = vc_audio::read_wav_file(&path).unwrap();
    assert_eq!(stored.samples.len(), 128_000);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        api.get("/v1/voices").await.json()["voices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let r = api.delete(&format!("/v1/voices/{id}")).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(!path.exists());
    assert_eq!(
        api.get(&format!("/v1/voices/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.delete(&format!("/v1/voices/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_job_runs_to_a_downloadable_wav() {
    let api = Api::fake();
    let voice = api.voice().await;
    let mut events = api.engine.subscribe();
    let text = "Hello there, this is the first sentence of a test. \
                Yeh doosra sentence hai, thoda Hinglish mein, aur ismein bahut saare words hain taaki chunks banein.";
    let r = api.job(&voice, "hi", text).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let job = r.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(r.headers[header::LOCATION], format!("/v1/jobs/{job}"));
    assert_eq!(r.json()["chunks_total"], 2);
    // The job may already have started on the free engine slot.
    assert!(["queued", "running", "succeeded"].contains(&r.json()["state"].as_str().unwrap()));

    api.wait_finished(&mut events, &job).await;
    let j = api.get(&format!("/v1/jobs/{job}")).await.json();
    assert_eq!(j["state"], "succeeded");
    assert_eq!(j["chunks_done"], 2);
    assert_eq!(j["chunks"].as_array().unwrap().len(), 2);
    assert_eq!(j["language"], "hi");
    assert!(j["timings"]["first_audio_ms"].is_i64());
    assert!(j["timings"]["run_ms"].is_i64());
    assert_eq!(j["provenance"]["backend_id"], "fake");
    assert!(j["expires_at"].is_string());
    assert!(j.get("text").is_none(), "the API never echoes job text");

    let a = api.get(&format!("/v1/jobs/{job}/audio")).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.headers[header::CONTENT_TYPE], "audio/wav");
    let pcm = vc_audio::decode_wav(std::io::Cursor::new(a.body.clone())).unwrap();
    assert_eq!(pcm.sample_rate, 24_000);
    use sha2::Digest;
    assert_eq!(
        j["provenance"]["output_sha256"],
        hex::encode(sha2::Sha256::digest(&a.body))
    );

    // Deleting an ended job removes it and its audio.
    assert_eq!(
        api.delete(&format!("/v1/jobs/{job}")).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.get(&format!("/v1/jobs/{job}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(!api.data.job_dir(job.parse().unwrap()).exists());
}

#[tokio::test]
async fn job_validation() {
    let api = Api::new(
        Arc::new(FakeBackend::with_limits(19, 10)),
        EngineConfig::default(),
        ApiConfig {
            max_job_chars: 50,
            ..ApiConfig::default()
        },
    );
    let voice = api.voice().await;
    let cases = [
        (
            json!({"voice_id": voice, "language": "fr", "text": "bonjour"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_language",
        ),
        (
            json!({"voice_id": voice, "language": "en", "text": "x".repeat(51)}),
            StatusCode::PAYLOAD_TOO_LARGE,
            "text_too_long",
        ),
        (
            json!({"voice_id": voice, "language": "en", "text": "  \n "}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json!({"voice_id": voice, "language": "en", "text": "supercalifragilistic"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsplittable_text",
        ),
        (
            json!({"voice_id": "voice_01ARZ3NDEKTSV4RRFFQ69G5FAV", "language": "en", "text": "hi"}),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            json!({"voice_id": "../etc", "language": "en", "text": "hi"}),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            json!({"voice_id": voice, "language": "en", "text": "hi", "seed": 42}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ];
    for (body, status, code) in cases {
        let r = api.post_json("/v1/jobs", body.clone()).await;
        assert_eq!((r.status, r.code()), (status, code.to_owned()), "{body}");
    }
    assert_eq!(
        api.get("/v1/jobs").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn cancel_running_audio_not_ready_and_voice_in_use() {
    let (api, mut ctl) = Api::gated(EngineConfig::default());
    let voice = api.voice().await;
    let job = api.job(&voice, "en", "Long running job.").await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ctl.next_started().await.unwrap();

    let a = api.get(&format!("/v1/jobs/{job}/audio")).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::CONFLICT, "job_not_ready".into())
    );
    let d = api.delete(&format!("/v1/voices/{voice}")).await;
    assert_eq!(
        (d.status, d.code()),
        (StatusCode::CONFLICT, "voice_in_use".into())
    );
    assert_eq!(
        api.get(&format!("/v1/jobs/{job}")).await.json()["state"],
        "running"
    );

    let c = api.delete(&format!("/v1/jobs/{job}")).await;
    assert_eq!(c.status, StatusCode::OK);
    assert_eq!(c.json()["state"], "cancelled");
    assert_eq!(c.json()["error_code"], "cancelled");
    // Now the voice is free.
    assert_eq!(
        api.delete(&format!("/v1/voices/{voice}")).await.status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn queue_position_and_queue_full() {
    let (api, mut ctl) = Api::gated(EngineConfig {
        queue_capacity: 1,
        chunk_estimate_s: 30,
        ..EngineConfig::default()
    });
    let voice = api.voice().await;
    let first = api.job(&voice, "en", "One.").await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ctl.next_started().await.unwrap();
    let second = api.job(&voice, "en", "Two.").await;
    assert_eq!(second.status, StatusCode::ACCEPTED);
    assert_eq!(second.json()["state"], "queued");
    assert_eq!(second.json()["queue_position"], 1);

    let third = api.job(&voice, "en", "Three.").await;
    assert_eq!(
        (third.status, third.code()),
        (StatusCode::TOO_MANY_REQUESTS, "queue_full".into())
    );
    assert_eq!(third.headers[header::RETRY_AFTER], "60");

    let mut events = api.engine.subscribe();
    ctl.release(2);
    api.wait_finished(&mut events, &first).await;
    let second = second.json()["id"].as_str().unwrap().to_owned();
    if api.get(&format!("/v1/jobs/{second}")).await.json()["state"] != "succeeded" {
        api.wait_finished(&mut events, &second).await;
    }
    let s: JobState = serde_json::from_value(
        api.get(&format!("/v1/jobs/{second}")).await.json()["state"].clone(),
    )
    .unwrap();
    assert_eq!(s, JobState::Succeeded);
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let api = Api::new(
        Arc::new(FakeBackend::new()),
        EngineConfig::default(),
        ApiConfig {
            max_body_bytes: 64 * 1024,
            ..ApiConfig::default()
        },
    );
    let consent = api.consent().await;
    let r = api
        .post_multipart(
            "/v1/voices",
            &[
                ("consent_id", consent.into_bytes()),
                ("file", wav(16_000, 8.0)),
            ],
        )
        .await;
    assert_eq!(
        (r.status, r.code()),
        (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large".into())
    );
}
