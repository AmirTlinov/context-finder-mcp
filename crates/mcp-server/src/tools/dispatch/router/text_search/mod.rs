mod budget;
mod cursor;
mod handler;
mod scan_corpus;
mod scan_filesystem;
mod types;

#[cfg(test)]
mod tests;

pub(in crate::tools::dispatch) use handler::text_search;
