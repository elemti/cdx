//! BROCODEX: end-to-end coverage for Bro's retry policy in `core/src/bro_retry.rs`.

use anyhow::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

fn recovered() -> ResponseTemplate {
    responses::sse_response(responses::sse(vec![
        responses::ev_response_created("recovered"),
        responses::ev_completed("recovered"),
    ]))
}

/// A provider without remote compaction, so `/compact` summarizes locally.
fn local_compaction_provider(server: &MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.name = "OpenAI (test)".into();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(0);
    provider
}

async fn build(server: &MockServer) -> Result<TestCodex> {
    test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_auto_env(server)
        .await
}

async fn submit_user_input(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    Ok(())
}

/// Collects retry statuses until the turn completes, failing on any terminal error.
async fn retry_statuses_until_completed(test: &TestCodex) -> Vec<String> {
    let mut statuses = Vec::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::StreamError(error) => statuses.push(error.message),
            EventMsg::Error(error) => panic!("unexpected terminal error: {error:?}"),
            EventMsg::TurnComplete(completed) => {
                assert_eq!(completed.error, None);
                return statuses;
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_rate_limit_retries_beyond_upstream_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let rate_limited = ResponseTemplate::new(429)
        .insert_header("Retry-After", "1")
        .set_body_json(json!({ "error": { "code": "rate_limit_exceeded" } }));
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![rate_limited.clone(), rate_limited, recovered()],
    )
    .await;
    let test = build(&server).await?;

    submit_user_input(&test, "retry the rate-limited request").await?;
    let statuses = retry_statuses_until_completed(&test).await;

    assert_eq!(
        statuses,
        [
            "Rate limited. Retrying in 1s...",
            "Rate limited. Retrying now...",
            "Rate limited. Retrying in 1s...",
            "Rate limited. Retrying now...",
        ]
    );
    assert_eq!(response_mock.requests().len(), 3);
    Ok(())
}

/// CPA reports mid-stream rate limits as `response.failed`, which carries no HTTP status.
#[tokio::test(flavor = "current_thread")]
async fn streamed_rate_limit_retries_as_rate_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "limited",
                "rate_limit_exceeded",
                "Rate limit reached. Please try again in 1s.",
            )),
            recovered(),
        ],
    )
    .await;
    let test = build(&server).await?;

    submit_user_input(&test, "retry the streamed rate limit").await?;
    let statuses = retry_statuses_until_completed(&test).await;

    assert_eq!(
        statuses,
        [
            "Rate limited. Retrying in 1s...",
            "Rate limited. Retrying now...",
        ]
    );
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// Upstream treats streamed overloads as terminal; the first retry waits the fixed 5s step.
#[tokio::test(flavor = "current_thread")]
async fn streamed_overload_retries_until_recovered() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "overloaded",
                "server_is_overloaded",
                "The model is at capacity.",
            )),
            recovered(),
        ],
    )
    .await;
    let test = build(&server).await?;

    submit_user_input(&test, "retry the overloaded model").await?;
    let statuses = retry_statuses_until_completed(&test).await;

    assert_eq!(
        statuses,
        [
            "Model at capacity. Retrying in 5s...",
            "Model at capacity. Retrying in 4s...",
            "Model at capacity. Retrying in 3s...",
            "Model at capacity. Retrying in 2s...",
            "Model at capacity. Retrying in 1s...",
            "Model at capacity. Retrying now...",
        ]
    );
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn usage_limit_retries_until_reset() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({
                    "error": {
                        "type": "usage_limit_reached",
                        "message": "The usage limit has been reached",
                    }
                })),
            recovered(),
        ],
    )
    .await;
    let test = build(&server).await?;

    submit_user_input(&test, "retry after the usage limit resets").await?;
    let statuses = retry_statuses_until_completed(&test).await;

    assert_eq!(
        statuses,
        [
            "Usage limit reached. Retrying in 1s...",
            "Usage limit reached. Retrying now...",
        ]
    );
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn quota_exceeded_stays_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_once(
        &server,
        ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "insufficient_quota",
                "message": "You exceeded your current quota.",
            }
        })),
    )
    .await;
    let test = build(&server).await?;

    submit_user_input(&test, "do not retry an exhausted quota").await?;
    let mut statuses = 0;
    let completion_error = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::StreamError(_) => statuses += 1,
            EventMsg::TurnComplete(completed) => {
                break completed.error.and_then(|error| error.codex_error_info);
            }
            _ => {}
        }
    };

    assert_eq!(statuses, 0);
    assert_eq!(completion_error, Some(CodexErrorInfo::UsageLimitExceeded));
    assert_eq!(response_mock.requests().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn local_compaction_retries_rate_limits() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("answer", "Done."),
                responses::ev_completed("answer"),
            ])),
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "rate_limit_exceeded" } })),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("summary", "Compacted summary."),
                responses::ev_completed("summary"),
            ])),
        ],
    )
    .await;
    let provider = local_compaction_provider(&server);
    let test = test_codex()
        .with_config(move |config| config.model_provider = provider)
        .build(&server)
        .await?;

    submit_user_input(&test, "leave something to compact").await?;
    assert_eq!(
        retry_statuses_until_completed(&test).await,
        Vec::<String>::new()
    );
    test.codex.submit(Op::Compact).await?;
    let statuses = retry_statuses_until_completed(&test).await;

    assert_eq!(
        statuses,
        [
            "Rate limited. Retrying in 1s...",
            "Rate limited. Retrying now...",
        ]
    );
    assert_eq!(response_mock.requests().len(), 3);
    Ok(())
}
