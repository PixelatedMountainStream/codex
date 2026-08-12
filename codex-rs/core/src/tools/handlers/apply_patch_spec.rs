use std::collections::BTreeMap;

use codex_tools::AdditionalProperties;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("apply_patch.lark");

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let definition = if include_environment_id {
        APPLY_PATCH_LARK_GRAMMAR.replace(
            "start: begin_patch hunk+ end_patch",
            "start: begin_patch environment_id? hunk+ end_patch\nenvironment_id: \"*** Environment ID: \" filename LF",
        )
    } else {
        APPLY_PATCH_LARK_GRAMMAR.to_string()
    };
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.".to_string(),
        defer_loading: None,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition,
        },
    })
}

const APPLY_PATCH_FUNCTION_DESCRIPTION: &str = "\
Apply one or more file changes using the Codex apply_patch envelope format. \
Pass the entire patch as a single string in the `input` field.

Patch format:
  *** Begin Patch
  [one or more hunks]
  *** End Patch

Supported hunk types:

  Add a new file:
    *** Add File: path/to/file.ext
    +line one
    +line two

  Update an existing file:
    *** Update File: path/to/file.ext
    @@ optional context description
    -old line
    +new line

  Delete a file:
    *** Delete File: path/to/file.ext

Lines prefixed with `+` are added; lines prefixed with `-` are removed; \
lines with no prefix are unchanged context lines. \
Multiple hunks of any type may appear between Begin Patch and End Patch.

Example:
  *** Begin Patch
  *** Add File: src/hello.rs
  +fn main() {
  +    println!(\"Hello, world!\");
  +}
  *** End Patch";

/// Returns a standard JSON-schema function tool for apply_patch.
/// Suitable for models that produce OpenAI-style `tool_calls` JSON.
pub fn create_apply_patch_function_tool(include_environment_id: bool) -> ToolSpec {
    let description = if include_environment_id {
        format!(
            "{APPLY_PATCH_FUNCTION_DESCRIPTION}\n\n\
             When working in a multi-environment session, begin the patch with \
             `*** Environment ID: <id>` on the first line after *** Begin Patch."
        )
    } else {
        APPLY_PATCH_FUNCTION_DESCRIPTION.to_string()
    };

    let mut properties = BTreeMap::new();
    properties.insert(
        "input".to_string(),
        JsonSchema::string(Some(
            "The full apply_patch envelope, starting with *** Begin Patch and ending with *** End Patch.".to_string(),
        )),
    );

    let parameters = JsonSchema::object(
        properties,
        Some(vec!["input".to_string()]),
        Some(AdditionalProperties::Boolean(false)),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "apply_patch".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "apply_patch_spec_tests.rs"]
mod tests;
