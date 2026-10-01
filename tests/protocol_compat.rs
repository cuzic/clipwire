#[path = "compat/old_register.rs"]
mod old_register;

use old_register::OldRegisterRequest;

/// AC-T3.6.5 (L): an old `flatten` parser drops the nested definition, while
/// the legacy flat request preserves it. This fixture must continue to model
/// the P2 parser independently of the production request types.
#[test]
fn old_server_parser_requires_the_flat_register_form() {
    let nested: OldRegisterRequest = serde_json::from_str(
        r#"{"name":"compat","target":{"script":"echo nested","env":{"A":"1"}}}"#,
    )
    .unwrap();
    assert_eq!(nested.name, "compat");
    assert!(nested.target.dir.is_none());
    assert!(nested.target.script.is_none());
    assert!(nested.target.steps.is_none());
    assert!(nested.target.env.is_empty());

    let flat: OldRegisterRequest =
        serde_json::from_str(r#"{"name":"compat","script":"echo flat","env":{"A":"1"}}"#).unwrap();
    assert_eq!(flat.target.script.as_deref(), Some("echo flat"));
    assert_eq!(flat.target.env.get("A").map(String::as_str), Some("1"));
}
