use super::super::build_context;
use super::support::base_request;
use std::path::PathBuf;

#[test]
fn build_context_reserves_headroom() {
    let mut request = base_request();
    request.max_chars = Some(20_000);

    let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
        .unwrap_or_else(|_| panic!("build_context should succeed"));
    assert_eq!(ctx.inner_max_chars, 19_200);
}

#[test]
fn build_context_never_exceeds_max_chars() {
    let mut request = base_request();
    request.max_chars = Some(500);

    let ctx = build_context(&request, PathBuf::from("."), ".".to_string())
        .unwrap_or_else(|_| panic!("build_context should succeed"));
    assert_eq!(ctx.max_chars, 500);
    assert_eq!(ctx.inner_max_chars, 436);
}
