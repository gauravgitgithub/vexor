//! Latency histogram for per-route stats: recorded by shims, merged and
//! stored by the daemon, read as quantiles by both.
//!
//! Bucket `i` holds durations from 0.1 ms × 2^(i/4) up to the next bucket
//! (bucket 0 also everything shorter), so a quantile read back is within
//! about 9% of the true value. 100 buckets reach half an hour. Kept sparse:
//! `(bucket, count)` pairs in bucket order.

pub const BUCKETS: u16 = 100;
const BASE_MS: f64 = 0.1;

/// The bucket a duration falls in.
pub fn bucket(ms: f64) -> u16 {
    if ms.is_nan() || ms <= BASE_MS {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let i = ((ms / BASE_MS).log2() * 4.0)
        .floor()
        .min(f64::from(BUCKETS - 1)) as u16;
    i
}

/// A duration standing for bucket `i` (its geometric middle).
pub fn value(i: u16) -> f64 {
    BASE_MS * 2f64.powf((f64::from(i) + 0.5) / 4.0)
}

/// Add `count` durations in bucket `b`.
pub fn add(hist: &mut Vec<(u16, u64)>, b: u16, count: u64) {
    match hist.binary_search_by_key(&b, |&(i, _)| i) {
        Ok(at) => hist[at].1 += count,
        Err(at) => hist.insert(at, (b, count)),
    }
}

pub fn merge(into: &mut Vec<(u16, u64)>, from: &[(u16, u64)]) {
    for &(b, n) in from {
        add(into, b, n);
    }
}

/// The `q` quantile (0..=1), or `None` for an empty histogram.
pub fn quantile(hist: &[(u16, u64)], q: f64) -> Option<f64> {
    let total: u64 = hist.iter().map(|&(_, n)| n).sum();
    if total == 0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
    let mut seen = 0;
    for &(b, n) in hist {
        seen += n;
        if seen >= rank {
            return Some(value(b));
        }
    }
    hist.last().map(|&(b, _)| value(b))
}

/// Compact storage form: per pair, the bucket as one byte and the count as
/// a LEB128 varint.
pub fn encode(hist: &[(u16, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hist.len() * 2);
    for &(b, mut n) in hist {
        out.push(u8::try_from(b.min(BUCKETS - 1)).unwrap_or(u8::MAX));
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    out
}

/// Inverse of [`encode`]; a truncated or malformed tail is ignored.
pub fn decode(mut bytes: &[u8]) -> Vec<(u16, u64)> {
    let mut out = Vec::new();
    'pairs: while let Some((&b, rest)) = bytes.split_first() {
        bytes = rest;
        let mut n = 0u64;
        let mut shift = 0;
        loop {
            let Some((&byte, rest)) = bytes.split_first() else {
                break 'pairs;
            };
            bytes = rest;
            if shift < 64 {
                n |= u64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        add(&mut out, u16::from(b), n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_within_ten_percent() {
        for ms in [0.2, 1.0, 3.7, 12.0, 250.0, 9_999.0, 600_000.0] {
            let v = value(bucket(ms));
            assert!((v - ms).abs() / ms < 0.1, "{ms} -> {v}");
        }
        assert_eq!(bucket(0.0), 0);
        assert_eq!(bucket(-5.0), 0);
        assert_eq!(bucket(f64::NAN), 0);
        assert_eq!(bucket(1e12), BUCKETS - 1);
    }

    #[test]
    fn quantiles_follow_ranks() {
        let mut h = Vec::new();
        for _ in 0..95 {
            add(&mut h, bucket(10.0), 1);
        }
        add(&mut h, bucket(1_000.0), 5);
        let p = |q| quantile(&h, q).expect("some");
        assert!((p(0.5) - 10.0).abs() < 1.0);
        assert!((p(0.95) - 10.0).abs() < 1.0);
        assert!((p(0.99) - 1_000.0).abs() < 100.0);
        assert_eq!(quantile(&[], 0.5), None);
    }

    #[test]
    fn encoding_round_trips_and_merges() {
        let mut h = vec![(0, 1), (40, 300), (99, u64::from(u32::MAX) + 7)];
        assert_eq!(decode(&encode(&h)), h);
        merge(&mut h, &[(40, 1), (41, 2)]);
        assert_eq!(
            h,
            vec![(0, 1), (40, 301), (41, 2), (99, u64::from(u32::MAX) + 7)]
        );
        // A cut-off varint is dropped, not misread.
        let bytes = encode(&[(5, 1_000)]);
        assert_eq!(decode(&bytes[..2]), vec![]);
    }
}
