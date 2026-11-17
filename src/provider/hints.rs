//! What to do about an error a known server sends (beyond Python).
//!
//! A [`ServerKind`] section's HTTP failures keep Python's `HTTP <status>: <body>`
//! text, and gain a hint when the body is one the server is known to send for a
//! problem the user can fix: a launch flag, the context length, the key. Bodies
//! are matched on the server's own wording, taken from its source (vLLM `main`
//! at `f39d833`, 2026-11-17), so an unrecognized body gets no hint rather than a
//! wrong one.

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
    }
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
