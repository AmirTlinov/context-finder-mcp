use crate::command::domain::RequestOptions;

pub fn is_active(options: &RequestOptions) -> bool {
    context_protocol::path_filters::is_active(
        &options.include_paths,
        &options.exclude_paths,
        options.file_pattern.as_deref(),
    )
}

pub fn path_allowed(rel_path: &str, options: &RequestOptions) -> bool {
    context_protocol::path_filters::path_allowed(
        rel_path,
        &options.include_paths,
        &options.exclude_paths,
        options.file_pattern.as_deref(),
    )
}
