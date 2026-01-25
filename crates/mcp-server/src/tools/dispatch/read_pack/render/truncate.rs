pub(in crate::tools::dispatch::read_pack) fn truncate_to_chars(
    input: &str,
    max_chars: usize,
) -> String {
    let max_chars = max_chars.max(1);
    let mut cut_byte = input.len();
    for (seen, (idx, _)) in input.char_indices().enumerate() {
        if seen == max_chars {
            cut_byte = idx;
            break;
        }
    }
    input[..cut_byte].to_string()
}
