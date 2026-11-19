//! What to do about an error a known server sends (beyond Python).
//!
//! A [`ServerKind`] section's HTTP failures keep Python's `HTTP <status>: <body>`
//! text, and gain a hint when the body is one the server is known to send for a
//! problem the user can fix: a launch flag, the context length, the key. Bodies
//! are matched on the server's own wording, taken from its source (vLLM `main`
//! at `f39d833`, 2026-11-17) or, for LM Studio, which documents none, from the
//! messages its bug tracker shows, so an unrecognized body gets no hint rather
//! than a wrong one.

use super::ServerKind;

/// Where the hint can point: the section's name and its key variable.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    pub name: &'a str,
    pub api_key_env: Option<&'a str>,
}

/// The hint for an HTTP failure from a server of `kind`, if its body is known.
pub fn for_http(kind: ServerKind, status: u16, body: &str, section: Section<'_>) -> Option<String> {
    match kind {
        ServerKind::Vllm => vllm(status, body, section),
        ServerKind::LmStudio => lmstudio(status, body, section),
    }
}

/// The hint the empty-answer path adds for LM Studio: one open report says a
/// GGUF model can answer an overflowing request with 200 and nothing streamed.
pub const LMSTUDIO_EMPTY_ANSWER: &str = "[LM Studio sends nothing when a request no longer \
     fits the context length the model was loaded with: reload the model with a larger \
     context length in LM Studio, or set [model] context_window so agent86 compacts sooner]";

const LMSTUDIO_CONTEXT: &str = "the conversation no longer fits the context length the model \
     was loaded with: reload it with a larger context length in LM Studio, or set [model] \
     context_window so agent86 compacts sooner";

/// LM Studio documents none of its error bodies, so these follow the messages
/// its bug tracker shows (0.4.x): two context overflows, in a 400 or a 500, and
/// `model_not_found`.
fn lmstudio(status: u16, body: &str, section: Section<'_>) -> Option<String> {
    if status == 401 {
        let env = section.api_key_env.unwrap_or("LM_API_TOKEN");
        return Some(format!(
            "this LM Studio server requires an API token (Server Settings, Require \
             Authentication): set {env}, or store the token with `agent86 keys set {}`",
            section.name
        ));
    }
    let low = body.to_lowercase();
    if low.contains("greater than the context length")
        || low.contains("context size has been exceeded")
        || (low.contains("context length") && low.contains("larger"))
    {
        return Some(LMSTUDIO_CONTEXT.to_owned());
    }
    if low.contains("model_not_found") || low.contains("not found in downloaded models") {
        return Some(
            "LM Studio has no model by this name: download it with `lms get <model>`, or \
             pick one it lists with /model"
                .to_owned(),
        );
    }
    None
}

fn vllm(status: u16, body: &str, section: Section<'_>) -> Option<String> {
    if status == 401 {
        // vLLM's middleware answers `{"error": "Unauthorized"}`, a bare string.
        let env = section.api_key_env.unwrap_or("VLLM_API_KEY");
        return Some(format!(
            "this vLLM server was started with --api-key: set {env}, or store the key \
             with `agent86 keys set {}`",
            section.name
        ));
    }
    // Sent whenever tools are present: an omitted tool_choice defaults to auto.
    if body.contains("tool choice requires --enable-auto-tool-choice")
        || body.contains("requires --tool-call-parser to be set")
    {
        return Some(
            "vLLM answers tool calls only when started with --enable-auto-tool-choice and a \
             --tool-call-parser that matches the model (see vLLM's tool calling docs); \
             restart it with both"
                .to_owned(),
        );
    }
    if body.contains("cannot be greater than max_model_len") {
        return Some(format!(
            "lower max_tokens in [providers.{}] below the server's --max-model-len",
            section.name
        ));
    }
    if body.contains("This model's maximum context length is")
        || body.contains("is longer than the maximum model length of")
    {
        return Some(
            "the conversation no longer fits the server's context length: start vLLM with a \
             larger --max-model-len if the model allows it, or set [model] context_window to \
             the server's length so agent86 compacts sooner"
                .to_owned(),
        );
    }
    if status == 404 && body.contains("does not exist") {
        return Some(
            "this server serves only the models it lists: pick one with /model, or start \
             vLLM with --served-model-name to serve this name"
                .to_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTION: Section<'static> = Section {
        name: "gpu-box",
        api_key_env: Some("GPU_BOX_API_KEY"),
    };

    fn hint(status: u16, body: &str) -> Option<String> {
        for_http(ServerKind::Vllm, status, body, SECTION)
    }

    // Bodies as vLLM's source builds them (`create_error_response`, the auth
    // middleware, and the Rust frontend's snapshot).
    #[test]
    fn vllm_bodies_get_the_hint_for_what_the_user_can_fix() {
        let unauthorized = hint(401, r#"{"error":"Unauthorized"}"#).unwrap();
        assert!(unauthorized.contains("--api-key"));
        assert!(unauthorized.contains("GPU_BOX_API_KEY"));
        assert!(unauthorized.contains("agent86 keys set gpu-box"));

        let tools = hint(
            400,
            r#"{"error":{"message":"\"auto\" tool choice requires --enable-auto-tool-choice and --tool-call-parser to be set","type":"BadRequestError","param":null,"code":400}}"#,
        )
        .unwrap();
        assert!(tools.contains("--enable-auto-tool-choice"));
        assert!(tools.contains("--tool-call-parser"));
        assert_eq!(
            hint(
                400,
                r#"{"error":{"message":"tool_choice=\"required\" requires --tool-call-parser to be set","type":"BadRequestError","param":null,"code":400}}"#,
            ),
            Some(tools)
        );

        let context = hint(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, your prompt contains 9000 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt.","type":"BadRequestError","param":"input_tokens","code":400}}"#,
        )
        .unwrap();
        assert!(context.contains("--max-model-len"));
        assert!(context.contains("[model] context_window"));
        assert_eq!(
            hint(
                400,
                r#"{"error":{"message":"The decoder prompt (length 8192) plus the number of requested output tokens (at least 1) is longer than the maximum model length of 8192. Make sure that `max_model_len` is no smaller than the number of text tokens plus multimodal tokens.","type":"BadRequestError","param":null,"code":400}}"#,
            ),
            Some(context)
        );

        let cap = hint(
            400,
            r#"{"error":{"message":"max_tokens=40000 cannot be greater than max_model_len=32768. Please request fewer output tokens.","type":"BadRequestError","param":null,"code":400}}"#,
        )
        .unwrap();
        assert!(cap.contains("[providers.gpu-box]"));

        let missing = hint(
            404,
            r#"{"error":{"message":"The model `qwen3` does not exist.","type":"NotFoundError","param":"model","code":404}}"#,
        )
        .unwrap();
        assert!(missing.contains("--served-model-name"));
    }

    #[test]
    fn an_unrecognized_body_gets_no_hint() {
        assert_eq!(hint(500, r#"{"error":{"message":"boom"}}"#), None);
        assert_eq!(hint(404, "Not Found"), None);
        assert_eq!(hint(400, ""), None);
    }

    // Bodies as LM Studio's bug tracker shows them (#2205, #1757, #1462); its
    // documentation describes none.
    #[test]
    fn lmstudio_bodies_get_the_hint_for_what_the_user_can_fix() {
        let hint = |status, body| {
            for_http(
                ServerKind::LmStudio,
                status,
                body,
                Section {
                    name: "lmstudio",
                    api_key_env: Some("LM_API_TOKEN"),
                },
            )
        };
        let keep = hint(
            400,
            r#"{"error":"The number of tokens to keep from the initial prompt is greater than the context length. Try to load the model with a larger context length, or provide a shorter input."}"#,
        )
        .unwrap();
        assert!(keep.contains("reload it with a larger context length"));
        assert_eq!(
            hint(500, r#"{"error":"Context size has been exceeded."}"#),
            Some(keep)
        );
        let missing = hint(
            404,
            r#"{"error":{"type":"model_not_found","message":"Model qwen3 not found in downloaded models"}}"#,
        )
        .unwrap();
        assert!(missing.contains("lms get"));
        let token = hint(401, "").unwrap();
        assert!(token.contains("LM_API_TOKEN") && token.contains("agent86 keys set lmstudio"));
        assert_eq!(hint(500, r#"{"error":"boom"}"#), None);
    }

    #[test]
    fn a_401_without_a_key_variable_names_vllms() {
        let hint = for_http(
            ServerKind::Vllm,
            401,
            "",
            Section {
                name: "vllm",
                api_key_env: None,
            },
        )
        .unwrap();
        assert!(hint.contains("set VLLM_API_KEY"));
    }
}
