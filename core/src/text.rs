//! 到处都要用的一点点字符串处理。
//!
//! 以前 `cap` / `one_line` / `host_of` 各在五六个模块里各写一份（`status.rs` 的
//! 叫 `cap`、`linux.rs` 叫 `clip_chars`、`notify.rs` 和 `cdp.rs` 叫 `clip`），
//! 改一处忘一处就会漂 —— 同一条通知和同一条状态行的截断规则不一样，看着像 bug。
//! 语义只有这几种，收在这里一份。

/// 按**字符**截断并加省略号。
///
/// 不按字节：中文一个字三字节，按字节切会切出半个字（界面上是个方块）。
pub fn cap(s: &str, max: usize) -> String {
    let Some((cut, _)) = s.char_indices().nth(max) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(cut + '…'.len_utf8());
    out.push_str(&s[..cut]);
    out.push('…');
    out
}

/// 把一段**网络分片**攒进缓冲区，凑齐完整帧再交给调用方解码。
///
/// 为什么不能一片一片 `from_utf8_lossy`：一个汉字三字节，被切在两次 `read`
/// 之间时，两片各自解码都变成 `\u{FFFD}`，中文就成了乱码（`openai.rs` 早踩过，
/// 那边的注释写着这件事；`machines.rs` / `alpine.rs` 后来各抄了一遍错的那版）。
/// 帧边界（空行）是纯 ASCII，按字节切是安全的，所以**先切帧、后解码**。
///
/// 这个结构只管「攒 + 切」，解码交给调用方（有的要整帧 `&str`，有的要按行）。
#[derive(Default)]
pub struct FrameBuf {
    buf: Vec<u8>,
    /// 已经取走的字节数。帧边界只往后找，积压的大半用完时整体搬一次 ——
    /// 每帧都 `drain` 的话，一次读回 1MB（几百帧）就是 O(n²) 的 memmove
    /// （实测 1MB 一批：69ms vs 0.5ms）。
    taken: usize,
}

impl FrameBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一片网络数据。
    ///
    /// 压紧（把已经取走的字节搬掉）放在这里、不放在 `next_frame`：那里要把当前帧
    /// 借给调用方，搬完就借不出来了（`drain` 掉的正好是它）。
    pub fn push(&mut self, chunk: &[u8]) {
        if self.taken > 0 && self.taken * 2 >= self.buf.len() {
            self.buf.drain(..self.taken);
            self.taken = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// 取出下一个完整帧（到空行为止，不含空行本身），没有就 `None`。
    ///
    /// 每次调用只推进游标，不搬字节 —— 一次读回几百帧时不会退化成 O(n²)
    /// （实测 1MB 一批：`Vec::drain` 版 69ms，游标版 0.5ms）。
    pub fn next_frame(&mut self) -> Option<&[u8]> {
        let (end, sep) = frame_end(&self.buf[self.taken..])?;
        let start = self.taken;
        self.taken += end + sep;
        Some(&self.buf[start..start + end])
    }

    /// 把剩下没成帧的尾巴整段取走（`None` = 只剩空白）。
    ///
    /// 有些服务端最后一条事件后面不发空行 —— 那一帧也得处理，不能丢。
    pub fn take_rest(&mut self) -> Option<&[u8]> {
        if self.buf[self.taken..].iter().all(u8::is_ascii_whitespace) {
            self.clear();
            return None;
        }
        let start = self.taken;
        self.taken = self.buf.len();
        Some(&self.buf[start..])
    }

    /// 还剩多少字节没成帧（用于「攒太多了，丢掉重来」的兜底）。
    pub fn pending_len(&self) -> usize {
        self.buf.len() - self.taken
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.taken = 0;
    }
}

/// 找第一个帧边界（空行）：返回 (帧长度, 分隔符长度)。`\n\n` 和 `\r\n\r\n` 都认。
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) if b < a => Some((b, 4)),
        (Some(a), _) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

/// 压成一行：连续空白折成一个空格，首尾不留空白。
///
/// 通知只有一段（多个换行会被系统折成一堆空白），登记表里也只需要一条线索。
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 去掉协议头之后那一截（到 `/`、`?`、`#` 为止），**是 `url` 的一段** ——
/// 调用方可以直接拿它的长度回原文上切片。
pub fn authority(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))
        .unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// 域名：小写，去掉 `user:pass@` 和端口。认不出时给空串。
pub fn host_name(url: &str) -> String {
    let auth = authority(url);
    let host = auth.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个汉字的三个字节被切在两次读之间时，**不能**变成两个替换符。
    ///
    /// 这是 `machines.rs` / `alpine.rs` 曾经的真实 bug：按每片网络数据各自
    /// `from_utf8_lossy` 解码，中文状态和 apk 中文输出都成了乱码。
    #[test]
    fn frame_boundary_split_does_not_garble_multibyte_chars() {
        // 「你」= E4 BD A0，切在第二个字节后面；帧以空行收尾
        let mut fb = FrameBuf::new();
        fb.push(&[b'd', b'a', b't', b'a', b':', b' ', 0xE4, 0xBD]);
        assert!(fb.next_frame().is_none(), "还没到空行，不该出帧");
        fb.push(&[0xA0, b'\n', b'\n']);
        let frame = fb.next_frame().expect("这一下应该成帧");
        assert_eq!(std::str::from_utf8(frame).unwrap(), "data: 你");
        assert!(fb.next_frame().is_none());
    }

    /// 两种帧分隔符都认；一帧一帧按顺序出；尾巴（没有收尾空行）能整段取走。
    #[test]
    fn frame_buf_handles_both_separators_and_the_tail() {
        let mut fb = FrameBuf::new();
        fb.push(b"a:1\n\nb:2\r\n\r\nc:3");
        assert_eq!(fb.next_frame().unwrap(), b"a:1");
        assert_eq!(fb.next_frame().unwrap(), b"b:2");
        assert!(fb.next_frame().is_none(), "最后一帧还没收尾");
        assert_eq!(fb.pending_len(), 3);
        assert_eq!(fb.take_rest().unwrap(), b"c:3");
        assert_eq!(fb.pending_len(), 0);
        // 只剩空白时 take_rest 给 None，且清干净
        fb.push(b"\n\n  ");
        assert!(fb.take_rest().is_none());
        assert_eq!(fb.pending_len(), 0);
    }

    /// 一次读回几百帧时不该退化成 O(n²)（这条是行为正确性，量级在 bench 里）。
    #[test]
    fn frame_buf_drains_many_frames_in_one_push() {
        const N: usize = 500;
        let mut fb = FrameBuf::new();
        let mut payload = Vec::new();
        for i in 0..N {
            payload.extend_from_slice(format!("data: {i}\n\n").as_bytes());
        }
        fb.push(&payload);
        for i in 0..N {
            let f = fb.next_frame().expect("每帧都该出来");
            assert_eq!(std::str::from_utf8(f).unwrap(), format!("data: {i}"));
        }
        assert!(fb.next_frame().is_none());
        assert_eq!(fb.pending_len(), 0);
    }

    #[test]
    fn cap_counts_characters_not_bytes() {
        assert_eq!(cap("报告", 5), "报告", "没超长就原样");
        assert_eq!(cap("报告文件", 2), "报告…", "按字符切，不留半个汉字");
        assert_eq!(cap("abcdef", 3), "abc…");
    }

    #[test]
    fn one_line_folds_whitespace() {
        assert_eq!(one_line("  a\n\n b\tc  "), "a b c");
        assert_eq!(one_line("   "), "");
    }

    #[test]
    fn host_name_is_the_bare_domain() {
        assert_eq!(
            host_name("https://API.OpenAI.com/v1/models"),
            "api.openai.com"
        );
        assert_eq!(
            host_name("http://user:pw@host.docker:8080/x"),
            "host.docker"
        );
        assert_eq!(host_name("//cdn.example.com/a.js"), "cdn.example.com");
        assert_eq!(host_name(""), "");
    }

    #[test]
    fn authority_is_a_slice_of_the_input() {
        let url = "https://example.com:443/a/b?q=1#frag";
        let a = authority(url);
        assert_eq!(a, "example.com:443");
        assert!(url.ends_with(&url[url.find(a).unwrap() + a.len()..]));
    }
}
