//! Program state recovered from the program's own event log.
//!
//! Some programs log their post-instruction state in Anchor events. For
//! those, the last event before a slot carries the economic fields of the
//! account at that slot, and the first event a transaction emits for an
//! account carries the pre-image the transaction started from, inverted
//! through the program's own transition rules. Nothing on chain keeps old
//! account bytes; events are the next best thing and they are free.
//!
//! Supported today: pump.fun bonding curves (`TradeEvent`: the four reserve
//! fields). The recovered fields are patched into the account's current
//! bytes; every other field stays current, and the certificate says so with
//! [`crate::Provenance::EventLog`].

use {solana_address::Address, std::str::FromStr};

/// The pump.fun bonding-curve program.
pub(crate) const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// `sha256("event:TradeEvent")[..8]`.
const TRADE_EVENT: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];
/// `sha256("account:BondingCurve")[..8]`.
const BONDING_CURVE: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// One pump.fun trade as the program logged it, with the four reserve
/// fields before and after (the "before" is the logged "after" inverted
/// through the program's documented buy/sell transition).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PumpTrade {
    pub mint: String,
    pub is_buy: bool,
    pub sol_amount: u64,
    pub token_amount: u64,
    /// `[virtual_token, virtual_sol, real_token, real_sol]` before the trade.
    pub pre: [u64; 4],
    /// The same four fields after the trade, as logged.
    pub post: [u64; 4],
}

fn u64_at(raw: &[u8], at: usize) -> Option<u64> {
    raw.get(at..at + 8)?.try_into().ok().map(u64::from_le_bytes)
}

/// Decode a `TradeEvent` payload, or `None` when the bytes are something else.
pub(crate) fn decode_trade(raw: &[u8]) -> Option<PumpTrade> {
    if raw.len() < 129 || raw[..8] != TRADE_EVENT {
        return None;
    }
    let mint = Address::from(<[u8; 32]>::try_from(&raw[8..40]).ok()?).to_string();
    let sol_amount = u64_at(raw, 40)?;
    let token_amount = u64_at(raw, 48)?;
    let is_buy = match raw[56] {
        0 => false,
        1 => true,
        _ => return None,
    };
    // Layout after `is_buy`: user (32) at 57, timestamp (i64) at 89, then
    // virtual_sol, virtual_token, real_sol, real_token at 97.
    let post_logged = [
        u64_at(raw, 97)?,
        u64_at(raw, 105)?,
        u64_at(raw, 113)?,
        u64_at(raw, 121)?,
    ];
    // Reorder to the account's field order: virtual_token, virtual_sol,
    // real_token, real_sol.
    let post = [
        post_logged[1],
        post_logged[0],
        post_logged[3],
        post_logged[2],
    ];
    // Buy: tokens leave the curve, SOL enters. Sell: the reverse. Invert.
    let pre = if is_buy {
        [
            post[0].checked_add(token_amount)?,
            post[1].checked_sub(sol_amount)?,
            post[2].checked_add(token_amount)?,
            post[3].checked_sub(sol_amount)?,
        ]
    } else {
        [
            post[0].checked_sub(token_amount)?,
            post[1].checked_add(sol_amount)?,
            post[2].checked_sub(token_amount)?,
            post[3].checked_add(sol_amount)?,
        ]
    };
    Some(PumpTrade {
        mint,
        is_buy,
        sol_amount,
        token_amount,
        pre,
        post,
    })
}

/// Every pump.fun trade a transaction's logs carry, in order. Events are
/// only taken from `Program data:` lines emitted while the pump program is
/// the innermost frame; a log stream that does not close cleanly (truncated
/// by the runtime's log limit) yields nothing, since later trades could be
/// missing.
pub(crate) fn pump_trades(logs: &[String]) -> Vec<PumpTrade> {
    use base64::Engine;
    let mut stack: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for line in logs {
        if let Some(rest) = line.strip_prefix("Program ") {
            if let Some((program, tail)) = rest.split_once(" invoke [") {
                let depth: usize = tail.trim_end_matches(']').parse().unwrap_or(0);
                if depth != stack.len() + 1 {
                    return Vec::new();
                }
                stack.push(program);
                continue;
            }
            if let Some(program) = rest.strip_suffix(" success") {
                if stack.pop() != Some(program) {
                    return Vec::new();
                }
                continue;
            }
            if let Some((program, _)) = rest.split_once(" failed: ") {
                if stack.pop() != Some(program) {
                    return Vec::new();
                }
                continue;
            }
            if let Some(b64) = rest.strip_prefix("data: ") {
                if stack.last() == Some(&PUMP_PROGRAM) {
                    if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                        if let Some(t) = decode_trade(&raw) {
                            out.push(t);
                        }
                    }
                }
            }
        }
    }
    if stack.is_empty() {
        out
    } else {
        Vec::new()
    }
}

/// The bonding-curve account for a mint.
pub(crate) fn bonding_curve_of(mint: &str) -> Option<String> {
    let mint = Address::from_str(mint).ok()?;
    let program = Address::from_str(PUMP_PROGRAM).ok()?;
    Some(
        Address::find_program_address(&[b"bonding-curve", mint.as_ref()], &program)
            .0
            .to_string(),
    )
}

/// Whether `data` is a pump.fun `BondingCurve` account.
pub(crate) fn is_bonding_curve(data: &[u8]) -> bool {
    data.len() >= 49 && data[..8] == BONDING_CURVE
}

/// The curve's bytes with the four reserve fields set to `reserves`
/// (`[virtual_token, virtual_sol, real_token, real_sol]`) and `complete`
/// cleared: a curve that traded was not complete. Every other byte is left
/// as given.
pub(crate) fn patch_bonding_curve(data: &[u8], reserves: [u64; 4]) -> Option<Vec<u8>> {
    if !is_bonding_curve(data) {
        return None;
    }
    let mut out = data.to_vec();
    for (i, v) in reserves.iter().enumerate() {
        let at = 8 + i * 8;
        out[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
    out[48] = 0;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn event(is_buy: bool, sol: u64, tokens: u64, post: [u64; 4]) -> Vec<u8> {
        // post here is in LOGGED order: virtual_sol, virtual_token, real_sol, real_token.
        let mut raw = Vec::new();
        raw.extend_from_slice(&TRADE_EVENT);
        raw.extend_from_slice(&[7u8; 32]);
        raw.extend_from_slice(&sol.to_le_bytes());
        raw.extend_from_slice(&tokens.to_le_bytes());
        raw.push(u8::from(is_buy));
        raw.extend_from_slice(&[9u8; 32]);
        raw.extend_from_slice(&1_700_000_000i64.to_le_bytes());
        for v in post {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        raw
    }

    #[test]
    fn a_buy_inverts_to_the_pre_trade_reserves() {
        let t = decode_trade(&event(true, 1_000, 50, [10_000, 900, 5_000, 400])).unwrap();
        assert!(t.is_buy);
        // account order: virtual_token, virtual_sol, real_token, real_sol
        assert_eq!(t.post, [900, 10_000, 400, 5_000]);
        assert_eq!(t.pre, [950, 9_000, 450, 4_000]);
    }

    #[test]
    fn a_sell_inverts_the_other_way() {
        let t = decode_trade(&event(false, 1_000, 50, [10_000, 900, 5_000, 400])).unwrap();
        assert_eq!(t.pre, [850, 11_000, 350, 6_000]);
    }

    #[test]
    fn other_payloads_and_bad_booleans_are_ignored() {
        assert!(decode_trade(&[0u8; 129]).is_none());
        let mut raw = event(true, 1, 1, [1, 1, 1, 1]);
        raw[56] = 2;
        assert!(decode_trade(&raw).is_none());
    }

    #[test]
    fn trades_are_taken_only_from_the_pump_frame_and_only_from_clean_logs() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(event(
            true,
            10,
            5,
            [100, 100, 100, 100],
        ));
        let logs = |close: bool| {
            let mut l = vec![
                format!("Program {PUMP_PROGRAM} invoke [1]"),
                "Program log: Instruction: Buy".to_string(),
                "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]".to_string(),
                format!("Program data: {b64}"), // inside the token frame: not pump's
                "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA success".to_string(),
                format!("Program data: {b64}"),
            ];
            if close {
                l.push(format!("Program {PUMP_PROGRAM} success"));
            }
            l
        };
        assert_eq!(pump_trades(&logs(true)).len(), 1);
        assert!(pump_trades(&logs(false)).is_empty());
    }

    #[test]
    fn patching_sets_reserves_and_clears_complete() {
        let mut data = vec![0u8; 100];
        data[..8].copy_from_slice(&BONDING_CURVE);
        data[48] = 1;
        let out = patch_bonding_curve(&data, [1, 2, 3, 4]).unwrap();
        assert_eq!(u64_at(&out, 8), Some(1));
        assert_eq!(u64_at(&out, 32), Some(4));
        assert_eq!(out[48], 0);
        assert!(patch_bonding_curve(&[0u8; 100], [1, 2, 3, 4]).is_none());
    }
}
