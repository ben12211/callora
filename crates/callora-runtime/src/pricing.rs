//! What the model's tokens cost. Prices come from `AGENT_PRICES` (dollars per million
//! tokens, `model=input/cached/output`, entries separated by commas):
//! `gpt-6-sol=2/0.2/10,gpt-6-luna=0.1/0.01/0.5`. Without a price, cost is unknown, never 0.

use std::collections::BTreeMap;

use crate::ports::Usage;

/// Model name → [input, cached input, output] dollars per million tokens.
pub type Prices = BTreeMap<String, [f64; 3]>;

pub fn parse_prices(raw: &str) -> Prices {
    raw.split(',')
        .filter_map(|entry| {
            let (model, prices) = entry.split_once('=')?;
            let p: Vec<f64> = prices.split('/').filter_map(|v| v.trim().parse().ok()).collect();
            (p.len() == 3).then(|| (model.trim().to_string(), [p[0], p[1], p[2]]))
        })
        .collect()
}

/// The cost of `usage` in dollars, when its model (or a snapshot of it, "gpt-6-sol-2026-08-01")
/// has a price.
pub fn cost(usage: &Usage, prices: &Prices) -> Option<f64> {
    let [input, cached, output] = prices
        .iter()
        .filter(|(m, _)| usage.model.starts_with(m.as_str()))
        .max_by_key(|(m, _)| m.len())
        .map(|(_, p)| *p)?;
    let uncached = usage.input.saturating_sub(usage.cached) as f64;
    Some((uncached * input + usage.cached as f64 * cached + usage.output as f64 * output) / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_turn_tokens_into_dollars() {
        let prices = parse_prices("gpt-6-sol=2/0.2/10, gpt-6-luna=0.1/0.01/0.5, broken=1/2");
        assert_eq!(prices.len(), 2, "a malformed entry is skipped");
        let u = Usage { model: "gpt-6-sol-2026-08-01".into(), input: 3000, cached: 2000, output: 50 };
        let c = cost(&u, &prices).unwrap();
        assert!((c - (1000.0 * 2.0 + 2000.0 * 0.2 + 50.0 * 10.0) / 1e6).abs() < 1e-12, "{c}");
        assert!(cost(&Usage { model: "gpt-4o".into(), ..u }, &prices).is_none(), "no price, no cost");
    }
}
