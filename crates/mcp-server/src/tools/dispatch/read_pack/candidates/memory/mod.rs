mod collect;
mod score;

pub(in crate::tools::dispatch::read_pack) use collect::collect_memory_file_candidates;
pub(in crate::tools::dispatch::read_pack) use score::config_candidate_score;
