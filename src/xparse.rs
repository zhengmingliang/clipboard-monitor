//! 解析 RECORD 扩展拦截到的客户端请求，只关心 ConvertSelection。

/// X11 核心协议里 ConvertSelection 的主操作码。
pub const CONVERT_SELECTION_OPCODE: u8 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertSelectionReq {
    pub requestor: u32,
    pub selection: u32,
    pub target: u32,
    pub property: u32,
    pub time: u32,
}

/// 从一段 RECORD `FromClient` 数据里抽出全部 ConvertSelection。
///
/// `swapped` 为真时，被记录客户端的字节序与本进程相反。
/// 返回解析出的请求，以及已完整消费的字节数——`data[consumed..]` 是
/// 被分块截断的请求残片，调用方可暂存并与下一块拼接。
pub fn parse_convert_selections(data: &[u8], swapped: bool) -> (Vec<ConvertSelectionReq>, usize) {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let major = data[i];
        let len16 = read_u16(&data[i + 2..i + 4], swapped) as usize;
        let total = if len16 == 0 {
            if i + 8 > data.len() {
                break;
            }
            let ext = read_u32(&data[i + 4..i + 8], swapped) as usize;
            ext.saturating_mul(4)
        } else {
            len16.saturating_mul(4)
        };
        if total < 4 || i + total > data.len() {
            break;
        }
        if major == CONVERT_SELECTION_OPCODE && total >= 24 {
            out.push(ConvertSelectionReq {
                requestor: read_u32(&data[i + 4..i + 8], swapped),
                selection: read_u32(&data[i + 8..i + 12], swapped),
                target: read_u32(&data[i + 12..i + 16], swapped),
                property: read_u32(&data[i + 16..i + 20], swapped),
                time: read_u32(&data[i + 20..i + 24], swapped),
            });
        }
        i += total;
    }
    (out, i)
}

/// 残片是否值得留给下一块拼接：按 4 字节对齐，且头部声明的请求长度
/// 超出当前数据。上限 64KB，防止异常数据无限累积。
pub fn partial_request_carry(rest: &[u8], swapped: bool) -> Option<Vec<u8>> {
    const MAX_CARRY: usize = 64 * 1024;
    if rest.is_empty() || !rest.len().is_multiple_of(4) || rest.len() > MAX_CARRY {
        return None;
    }
    let len16 = read_u16(&rest[2..4], swapped) as usize;
    let claimed = if len16 == 0 {
        if rest.len() < 8 {
            // 扩展头还需 4 字节才能读总长，先保留。
            return Some(rest.to_vec());
        }
        let ext = read_u32(&rest[4..8], swapped) as usize;
        ext.saturating_mul(4)
    } else {
        len16.saturating_mul(4)
    };
    if claimed > rest.len() && claimed <= MAX_CARRY {
        Some(rest.to_vec())
    } else {
        None
    }
}

fn read_u16(b: &[u8], swapped: bool) -> u16 {
    let v = u16::from_ne_bytes([b[0], b[1]]);
    if swapped {
        v.swap_bytes()
    } else {
        v
    }
}

fn read_u32(b: &[u8], swapped: bool) -> u32 {
    let v = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
    if swapped {
        v.swap_bytes()
    } else {
        v
    }
}

/// 资源 ID 是否属于给定客户端（X11 resource-id-base / mask）。
pub fn xid_belongs(xid: u32, base: u32, mask: u32) -> bool {
    mask != 0 && (xid & !mask) == (base & !mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u16(buf: &mut [u8], v: u16) {
        buf.copy_from_slice(&v.to_ne_bytes());
    }

    fn put_u32(buf: &mut [u8], v: u32) {
        buf.copy_from_slice(&v.to_ne_bytes());
    }

    #[test]
    fn parses_native_convert_selection() {
        let mut raw = vec![0u8; 24];
        raw[0] = CONVERT_SELECTION_OPCODE;
        put_u16(&mut raw[2..4], 6);
        put_u32(&mut raw[4..8], 0x11);
        put_u32(&mut raw[8..12], 0x22);
        put_u32(&mut raw[12..16], 0x33);
        put_u32(&mut raw[16..20], 0x44);
        put_u32(&mut raw[20..24], 0x55);
        let (got, consumed) = parse_convert_selections(&raw, false);
        assert_eq!(consumed, 24);
        assert_eq!(
            got,
            vec![ConvertSelectionReq {
                requestor: 0x11,
                selection: 0x22,
                target: 0x33,
                property: 0x44,
                time: 0x55,
            }]
        );
    }

    #[test]
    fn skips_other_requests_and_keeps_alignment() {
        let mut raw = vec![0u8; 8 + 24];
        raw[0] = 3; // GetWindowAttributes, length 2
        put_u16(&mut raw[2..4], 2);
        put_u32(&mut raw[4..8], 99);
        raw[8] = CONVERT_SELECTION_OPCODE;
        put_u16(&mut raw[10..12], 6);
        put_u32(&mut raw[12..16], 7);
        put_u32(&mut raw[16..20], 8);
        put_u32(&mut raw[20..24], 9);
        put_u32(&mut raw[24..28], 10);
        put_u32(&mut raw[28..32], 11);
        let (got, consumed) = parse_convert_selections(&raw, false);
        assert_eq!(consumed, 32);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].requestor, 7);
        assert_eq!(got[0].target, 9);
    }

    #[test]
    fn swapped_bytes_are_decoded() {
        let mut native = vec![0u8; 24];
        native[0] = CONVERT_SELECTION_OPCODE;
        put_u16(&mut native[2..4], 6);
        put_u32(&mut native[4..8], 0x0102_0304);
        let mut swapped = native.clone();
        for chunk in swapped[2..].chunks_mut(4) {
            chunk.reverse();
        }
        // length is 2 bytes sitting in the first half of the first 4-byte word.
        // Reversing the whole word also reverses the opcode word's padding.
        // Rebuild swapped header explicitly instead.
        let mut raw = vec![0u8; 24];
        raw[0] = CONVERT_SELECTION_OPCODE;
        raw[1] = 0;
        let len = 6u16.swap_bytes().to_ne_bytes();
        // If native is LE, swap_bytes makes the numeric value BE on the wire
        // relative to us. read_u16(swapped=true) does from_ne + swap, so the
        // stored bytes must be the byte-reversed native encoding.
        raw[2] = len[0];
        raw[3] = len[1];
        // Wait: 6u16.swap_bytes().to_ne_bytes() is NOT the same as reversing
        // the native bytes when we then swap again. Store reversed native bytes.
        let len_ne = 6u16.to_ne_bytes();
        raw[2] = len_ne[1];
        raw[3] = len_ne[0];
        let req = 0x0102_0304u32.to_ne_bytes();
        raw[4] = req[3];
        raw[5] = req[2];
        raw[6] = req[1];
        raw[7] = req[0];
        let (got, consumed) = parse_convert_selections(&raw, true);
        assert_eq!(consumed, 24);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].requestor, 0x0102_0304);
    }

    #[test]
    fn split_request_is_assembled_from_carry() {
        let mut raw = [0u8; 24];
        raw[0] = CONVERT_SELECTION_OPCODE;
        put_u16(&mut raw[2..4], 6);
        put_u32(&mut raw[4..8], 0x1111);
        put_u32(&mut raw[8..12], 0x2222);
        put_u32(&mut raw[12..16], 0x3333);
        put_u32(&mut raw[16..20], 0x4444);
        put_u32(&mut raw[20..24], 0x5555);

        // 第一块只有前 8 字节：解析不出请求，但应留下残片。
        let (first, consumed) = parse_convert_selections(&raw[..8], false);
        assert!(first.is_empty());
        assert_eq!(consumed, 0);
        let carry = partial_request_carry(&raw[..8], false).expect("残片应保留");

        // 第二块补齐剩余 16 字节，拼上残片后应解出完整请求。
        let mut joined = carry;
        joined.extend_from_slice(&raw[8..]);
        let (second, consumed2) = parse_convert_selections(&joined, false);
        assert_eq!(consumed2, 24);
        assert_eq!(
            second,
            vec![ConvertSelectionReq {
                requestor: 0x1111,
                selection: 0x2222,
                target: 0x3333,
                property: 0x4444,
                time: 0x5555,
            }]
        );
    }

    #[test]
    fn zero_length_garbage_is_not_carried() {
        // len16=0 且扩展长度=0 的垃圾尾巴不应被保留，避免无限累积。
        let mut raw = [0u8; 32];
        raw[0] = CONVERT_SELECTION_OPCODE;
        put_u16(&mut raw[2..4], 6);
        put_u32(&mut raw[4..8], 0x11);
        let (reqs, consumed) = parse_convert_selections(&raw, false);
        assert_eq!(reqs.len(), 1);
        assert_eq!(consumed, 24);
        assert!(partial_request_carry(&raw[consumed..], false).is_none());
    }

    #[test]
    fn xid_base_match() {
        let base = 0x0320_0000;
        let mask = 0x001f_ffff;
        assert!(xid_belongs(base | 42, base, mask));
        assert!(!xid_belongs(0x0400_0000 | 42, base, mask));
    }
}
