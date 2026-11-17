/// Lowercases a title and strips accents and punctuation so search can compare titles.
pub fn fold_title(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

/// Writes a string with its length in front so concatenated keys cannot collide.
pub fn write_length_prefixed(out: &mut String, s: &str) {
    out.push_str(&s.len().to_string());
    out.push(':');
    out.push_str(s);
}
