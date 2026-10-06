use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_model_provider_info::WireApi;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::extensions::compression::deflate::DeflateConfig;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::http::Method;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[test_case::test_case(false; "local_compaction")]
#[test_case::test_case(true; "remote_compaction")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_size_close_replays_compaction_over_http_without_retries(
    remote_compaction: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let compact_output = if remote_compaction {
        serde_json::json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "compacted"}
        })
    } else {
        responses::ev_assistant_message("summary", "compacted")
    };
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![compact_output, ev_completed("compact-http")]),
            sse(vec![ev_completed("follow-up-http")]),
        ],
    )
    .await;

    // Serve both transports on one address, forwarding HTTP to the existing mock.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let http_address = server.address().to_owned();
    let websocket_requests = Arc::new(AtomicUsize::new(0));
    let request_count = Arc::clone(&websocket_requests);
    let proxy = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept test connection");
            let request_count = Arc::clone(&request_count);
            tokio::spawn(async move {
                let mut prefix = [0; 4];
                socket.peek(&mut prefix).await.expect("peek HTTP method");
                if !prefix.starts_with(b"GET ") {
                    let mut upstream = TcpStream::connect(http_address).await.expect("HTTP mock");
                    tokio::io::copy_bidirectional(&mut socket, &mut upstream)
                        .await
                        .expect("forward HTTP request");
                    return;
                }
                let mut websocket_config = WebSocketConfig::default();
                websocket_config.extensions.permessage_deflate = Some(DeflateConfig::default());
                let mut websocket = accept_async_with_config(socket, Some(websocket_config))
                    .await
                    .expect("websocket handshake");
                while let Some(Ok(Message::Text(text))) = websocket.next().await {
                    let request: serde_json::Value = serde_json::from_str(&text).expect("request");
                    if request["generate"] != false
                        && request_count.fetch_add(1, Ordering::SeqCst) > 0
                    {
                        websocket
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Size,
                                reason: "message too big".into(),
                            })))
                            .await
                            .expect("size rejection");
                        return;
                    }
                    for event in [ev_response_created("initial"), ev_completed("initial")] {
                        websocket
                            .send(Message::Text(event.to_string().into()))
                            .await
                            .expect("response");
                    }
                }
            });
        }
    });

    let mut builder = test_codex().with_config(move |config| {
        config.model_provider.base_url = Some(format!("http://{address}/v1"));
        config.model_provider.name = if remote_compaction {
            "OpenAI"
        } else {
            "size-test"
        }
        .into();
        config.model_provider.requires_openai_auth = false;
        config.model_provider.supports_websockets = true;
        // Recovery must work even when the ordinary retry budget is zero.
        config.model_provider.stream_max_retries = Some(0);
        config.model_provider.request_max_retries = Some(0);
    });
    let test = builder.build(&server).await?;
    test.submit_turn("before compact").await?;
    test.codex.submit(Op::Compact).await?;
    let mut compact_completed = false;
    loop {
        let event = timeout(Duration::from_secs(10), test.codex.next_event()).await??;
        match event.msg {
            EventMsg::Error(error) => panic!("compaction failed: {error:?}"),
            EventMsg::StreamError(error) => panic!("unexpected retry: {error:?}"),
            EventMsg::ItemCompleted(item) => {
                compact_completed |= matches!(
                    item.item,
                    codex_protocol::items::TurnItem::ContextCompaction(_)
                );
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(
        compact_completed,
        "compaction should complete after HTTP replay"
    );
    test.submit_turn("after compact").await?;
    assert_eq!(websocket_requests.load(Ordering::SeqCst), 2);
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let compact_request = requests[0].body_json().to_string();
    assert!(
        compact_request.contains("before compact"),
        "HTTP replay must preserve history"
    );
    assert!(
        requests[0]
            .body_json()
            .get("previous_response_id")
            .is_none()
    );
    proxy.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_on_upgrade_required_connect() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;

    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            // If we don't treat 426 specially, the sampling loop would retry the WebSocket
            // handshake before switching to the HTTP transport.
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The startup prewarm request sees 426 and immediately switches the session to HTTP fallback,
    // so the first turn goes straight to HTTP with no additional websocket connect attempt.
    assert_eq!(websocket_attempts, 1);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_after_retries_exhausted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // Deferred request prewarm is attempted at startup.
    // The first turn then makes 3 websocket stream attempts (initial try + 2 retries),
    // after which fallback activates and the request is replayed over HTTP.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_hides_first_websocket_retry_stream_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let TestCodex {
        codex,
        session_configured,
        cwd,
        ..
    } = builder.build(&server).await?;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;

    let mut stream_error_messages = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("event stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::StreamError(e) => stream_error_messages.push(e.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let expected_stream_errors = if cfg!(debug_assertions) {
        vec!["Reconnecting... 1/2", "Reconnecting... 2/2"]
    } else {
        vec!["Reconnecting... 2/2"]
    };
    assert_eq!(stream_error_messages, expected_stream_errors);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_is_sticky_across_turns() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("first").await?;
    test.submit_turn("second").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // WebSocket attempts all happen on the first turn:
    // 1 deferred request prewarm attempt (startup) + 3 stream attempts
    // (initial try + 2 retries) before fallback.
    // Fallback is sticky, so the second turn stays on HTTP and adds no websocket attempts.
    assert_eq!(websocket_attempts, 4);
    assert_eq!(http_attempts, 2);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}
