// Algorithm reference: Michael F. Schinder, "Branch, Call, and Jump: A New Compression Technique" (1994), as described in the LZMA SDK x86_Convert documentation.

pub const BASE_IP_ZERO: u64 = 0;
pub const BASE_X86: u64 = 0x0040_0000;
pub const BASE_X64: u64 = 0x0000_0001_4000_0000;

fn transform(
    input: &[u8],
    base: u64,
    to_absolute: bool,
    allow_terminal_truncation: bool,
) -> Result<Vec<u8>, String> {
    let mut output = input.to_vec();
    let mut position = 0usize;

    while position < input.len() {
        let opcode = input[position];
        let operand_position = if matches!(opcode, 0xE8 | 0xE9) {
            Some(position + 1)
        } else if opcode == 0x0F
            && position + 1 < input.len()
            && (input[position + 1] & 0xF0) == 0x80
        {
            Some(position + 2)
        } else {
            None
        };

        let Some(operand_position) = operand_position else {
            position += 1;
            continue;
        };
        let instruction_length = if opcode == 0x0F { 6 } else { 5 };
        let end = operand_position
            .checked_add(4)
            .ok_or("bcj: operand position overflow")?;
        if end > input.len() {
            if allow_terminal_truncation {
                break;
            }
            return Err(format!(
                "bcj: truncated branch operand at offset {position}"
            ));
        }
        let position_after = (position + instruction_length) as u64;
        let running_ip = base
            .checked_add(position_after)
            .ok_or("bcj: base plus instruction pointer overflow")?;
        let operand = u32::from_le_bytes([
            input[operand_position],
            input[operand_position + 1],
            input[operand_position + 2],
            input[operand_position + 3],
        ]);
        let replacement = if to_absolute {
            operand.wrapping_add(running_ip as u32)
        } else {
            operand.wrapping_sub(running_ip as u32)
        };
        output[operand_position..end].copy_from_slice(&replacement.to_le_bytes());
        position = end;
    }

    Ok(output)
}

pub fn forward(input: &[u8], base: u64) -> Result<Vec<u8>, String> {
    transform(input, base, true, false)
}

pub fn inverse(input: &[u8], base: u64) -> Result<Vec<u8>, String> {
    transform(input, base, false, false)
}

pub(crate) fn forward_terminal(input: &[u8], base: u64) -> Result<Vec<u8>, String> {
    transform(input, base, true, true)
}

pub(crate) fn inverse_terminal(input: &[u8], base: u64) -> Result<Vec<u8>, String> {
    transform(input, base, false, true)
}

#[cfg(test)]
mod tests {
    use super::{forward, forward_terminal, inverse, inverse_terminal, BASE_IP_ZERO, BASE_X64};

    fn assert_roundtrip(raw: &[u8], base: u64) {
        let filtered = forward(raw, base).expect("forward");
        let restored = inverse(&filtered, base).expect("inverse");
        assert_eq!(restored, raw);
    }

    #[test]
    fn exact_call_jump_and_jcc_vectors() {
        let call = [0x90, 0xE8, 0xFB, 0xFF, 0xFF, 0xFF, 0xCC];
        let call_expected = [0x90, 0xE8, 0x01, 0x00, 0x00, 0x00, 0xCC];
        assert_eq!(forward(&call, 0).unwrap(), call_expected);

        let jump = [0xE9, 0x10, 0x00, 0x00, 0x00];
        let jump_expected = [0xE9, 0x15, 0x00, 0x00, 0x00];
        assert_eq!(forward(&jump, 0).unwrap(), jump_expected);

        let jcc = [0x0F, 0x84, 0xFB, 0xFF, 0xFF, 0xFF];
        let jcc_expected = [0x0F, 0x84, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(forward(&jcc, 0).unwrap(), jcc_expected);
    }

    #[test]
    fn all_near_jcc_opcodes_roundtrip() {
        for condition in 0x80..=0x8F {
            let raw = [0x0F, condition, 0x01, 0x00, 0x00, 0x00];
            let expected = [0x0F, condition, 0x07, 0x00, 0x00, 0x00];
            assert_eq!(forward(&raw, BASE_IP_ZERO).unwrap(), expected);
            assert_roundtrip(&raw, BASE_IP_ZERO);
        }
    }

    #[test]
    fn empty_tiny_and_opcode_free_streams_are_identity() {
        for raw in [
            &[][..],
            &[0x90][..],
            &[0x0F, 0x90, 0x01][..],
            b"AAHL no branch opcodes",
        ] {
            assert_eq!(forward(raw, 0).unwrap(), raw);
            assert_eq!(inverse(raw, 0).unwrap(), raw);
            assert_roundtrip(raw, BASE_IP_ZERO);
        }
    }

    #[test]
    fn truncated_branch_operands_return_errors() {
        for raw in [
            &[0xE8][..],
            &[0xE8, 1, 2, 3][..],
            &[0xE9, 1, 2, 3][..],
            &[0x0F, 0x80][..],
            &[0x0F, 0x8F, 1, 2, 3][..],
        ] {
            assert!(forward(raw, 0).is_err(), "forward accepted {raw:?}");
            assert!(inverse(raw, 0).is_err(), "inverse accepted {raw:?}");
        }
    }

    #[test]
    fn terminal_chunk_adapter_preserves_incomplete_tail() {
        for raw in [
            &[0xE8][..],
            &[0xE8, 1, 2, 3][..],
            &[0x0F, 0x84, 1, 2, 3][..],
        ] {
            let filtered = forward_terminal(raw, BASE_IP_ZERO).unwrap();
            assert_eq!(filtered, raw);
            assert_eq!(inverse_terminal(&filtered, BASE_IP_ZERO).unwrap(), raw);
            assert!(forward(raw, BASE_IP_ZERO).is_err());
            assert!(inverse(raw, BASE_IP_ZERO).is_err());
        }
    }

    #[test]
    fn zero_relative_operand_uses_the_running_ip() {
        let raw = [0xE8, 0, 0, 0, 0];
        let expected = [0xE8, 0x05, 0, 0, 0];
        assert_eq!(forward(&raw, 0).unwrap(), expected);
        assert_roundtrip(&raw, 0);
    }

    #[test]
    fn scanner_advances_past_complete_branch_operands() {
        let raw = [0xE8, 0xE8, 0, 0, 0];
        let expected = [0xE8, 0xED, 0, 0, 0];
        assert_eq!(forward(&raw, 0).unwrap(), expected);
        assert_roundtrip(&raw, 0);
    }

    #[test]
    fn all_bases_roundtrip_deterministically() {
        let raw = [
            0x90, 0xE8, 0xF0, 0xFF, 0xFF, 0xFF, 0x0F, 0x85, 0x10, 0, 0, 0,
        ];
        for base in [0, 0x0040_0000, BASE_X64] {
            let first = forward(&raw, base).unwrap();
            let second = forward(&raw, base).unwrap();
            assert_eq!(first, second);
            assert_eq!(inverse(&first, base).unwrap(), raw);
        }
    }

    #[test]
    fn x64_high_base_uses_its_low32_address_bits() {
        let raw = [0xE8, 0, 0, 0, 0];
        let expected = [0xE8, 0x05, 0, 0, 0x40];
        assert_eq!(forward(&raw, BASE_X64).unwrap(), expected);
        assert_roundtrip(&raw, BASE_X64);
    }
}
