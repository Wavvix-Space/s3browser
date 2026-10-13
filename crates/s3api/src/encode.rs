//! S3 要求的 URL 编码：RFC 3986 unreserved 集合之外一律百分号编码，`~` 不编码。

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// 编码 URL 路径，保留分隔符 `/`。
pub fn encode_path(s: &str) -> String {
    encode(s, true)
}

/// 编码查询参数的名与值，`/` 也要编码。
pub fn encode_query(s: &str) -> String {
    encode(s, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_keeps_slash_and_encodes_the_rest() {
        assert_eq!(encode_path("a/b c.txt"), "a/b%20c.txt");
        assert_eq!(encode_path("test$file.text"), "test%24file.text");
        assert_eq!(encode_path("~-._"), "~-._");
        assert_eq!(encode_path("中"), "%E4%B8%AD");
        assert_eq!(encode_path("a+b&c=d"), "a%2Bb%26c%3Dd");
    }

    #[test]
    fn query_encodes_slash() {
        assert_eq!(
            encode_query("AKIA/20130524/us-east-1/s3/aws4_request"),
            "AKIA%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"
        );
        assert_eq!(encode_query("a b"), "a%20b");
    }
}
