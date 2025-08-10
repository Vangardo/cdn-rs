use regex::Regex;
use uuid::Uuid;

pub fn is_guid(s: &str) -> bool {
    // точное совпадение с Python-паттерном
    static GUID_RE: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"^[a-fA-F0-9]{8}-([a-fA-F0-9]{4}-){3}[a-fA-F0-9]{12}$").unwrap()
    });
    GUID_RE.is_match(s)
}

pub fn parse_guid(s: &str) -> Option<Uuid> {
    Uuid::parse_str(s).ok()
}
