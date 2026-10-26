//! API list prices for a model call, from LiteLLM's price table (the copy T3
//! Code keeps in `usage-model-rates.json`). Ported from T3's
//! `usagePricing.ts`, so a day here costs what T3's usage page says.

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRate {
    pub standard: TokenRates,
    pub fast: Option<TokenRates>,
    pub ultrafast: Option<TokenRates>,
}

/// Prices by lower-cased model name, plus the bare name (after the last `/`)
/// when every provider prefix agrees on it.
#[derive(Debug, Default)]
pub struct RateTable(HashMap<String, ModelRate>);

/// Tokens of one call (or a sum of calls on one model at one speed).
#[derive(Debug, Clone, Copy, Default)]
pub struct Tokens {
    pub uncached: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Reasoning included; never charged twice.
    pub output: u64,
}

/// `<synthetic>` was never billed; bare family names are ambiguous across
/// generations, so they stay unpriced rather than guessed.
const UNPRICEABLE: [&str; 6] = [
    "<synthetic>",
    "synthetic",
    "opus",
    "sonnet",
    "haiku",
    "fable",
];

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// A tier's rates. Cache rates missing from a tier keep the standard tier's
/// ratio to input; with no standard tier they cost as plain input.
fn read_rates(entry: &Value, suffix: &str, standard: Option<&TokenRates>) -> Option<TokenRates> {
    let input = num(entry, &format!("input_cost_per_token{suffix}"))?;
    let output = num(entry, &format!("output_cost_per_token{suffix}"))?;
    let cache = |name: &str, pick: fn(&TokenRates) -> f64| {
        num(entry, &format!("{name}{suffix}")).unwrap_or(match standard {
            Some(s) if s.input > 0.0 => pick(s) / s.input * input,
            _ => input,
        })
    };
    Some(TokenRates {
        input,
        output,
        cache_read: cache("cache_read_input_token_cost", |r| r.cache_read),
        cache_write: cache("cache_creation_input_token_cost", |r| r.cache_write),
    })
}

fn scale(r: &TokenRates, k: f64) -> TokenRates {
    TokenRates {
        input: r.input * k,
        output: r.output * k,
        cache_read: r.cache_read * k,
        cache_write: r.cache_write * k,
    }
}

fn bare(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

impl RateTable {
    /// From LiteLLM's JSON document (model name → entry).
    pub fn parse(doc: &Value) -> Self {
        let mut table = HashMap::new();
        let Some(map) = doc.as_object() else {
            return Self(table);
        };
        for (name, entry) in map {
            let Some(standard) = read_rates(entry, "", None) else {
                continue;
            };
            let key = name.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let multiple = entry
                .get("provider_specific_entry")
                .and_then(|p| p.get("fast"))
                .and_then(Value::as_f64)
                .filter(|m| m.is_finite() && *m > 0.0);
            table.insert(
                key,
                ModelRate {
                    standard,
                    fast: match multiple {
                        Some(m) => Some(scale(&standard, m)),
                        None => read_rates(entry, "_priority", Some(&standard)),
                    },
                    ultrafast: read_rates(entry, "_ultrafast", Some(&standard)),
                },
            );
        }
        // Bare aliases, only when unambiguous.
        let mut aliases: HashMap<String, Option<ModelRate>> = HashMap::new();
        for (key, rate) in &table {
            let alias = bare(key);
            if alias.is_empty() || alias == key || table.contains_key(alias) {
                continue;
            }
            match aliases.get(alias) {
                None => {
                    aliases.insert(alias.to_owned(), Some(*rate));
                }
                Some(Some(held)) if held != rate => {
                    aliases.insert(alias.to_owned(), None);
                }
                _ => {}
            }
        }
        for (alias, rate) in aliases {
            if let Some(rate) = rate {
                table.insert(alias, rate);
            }
        }
        Self(table)
    }

    pub fn lookup(&self, model: &str) -> Option<&ModelRate> {
        let key = model.trim().to_lowercase();
        // `claude-fable-5-1[1m]`: the 1M-context variant, priced at the base tier.
        let key = key.split('[').next().unwrap_or(&key);
        let b = bare(key);
        if b.is_empty() || UNPRICEABLE.contains(&b) {
            return None;
        }
        self.0.get(key)
    }

    /// List price in USD, or None when the model has no price. `speed`: 0
    /// standard, 1 fast, 2 ultrafast.
    pub fn price(&self, model: &str, speed: u8, t: &Tokens) -> Option<f64> {
        let rate = self.lookup(model)?;
        let r = match speed {
            1 => rate.fast.unwrap_or(rate.standard),
            2 => rate.ultrafast.unwrap_or(rate.standard),
            _ => rate.standard,
        };
        Some(
            t.uncached as f64 * r.input
                + t.cache_read as f64 * r.cache_read
                + t.cache_write as f64 * r.cache_write
                + t.output as f64 * r.output,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prices_like_t3() {
        let doc = json!({
            "claude-opus-5-5": {"input_cost_per_token": 5e-6, "output_cost_per_token": 25e-6,
                "cache_read_input_token_cost": 5e-7, "cache_creation_input_token_cost": 6.25e-6,
                "provider_specific_entry": {"fast": 2.0}},
            "azure/gpt-6-luna": {"input_cost_per_token": 1e-7, "output_cost_per_token": 5e-7,
                "cache_read_input_token_cost": 1e-8},
            "azure_ai/gpt-6-luna": {"input_cost_per_token": 1e-7, "output_cost_per_token": 5e-7,
                "cache_read_input_token_cost": 1e-8},
            "a/x": {"input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6},
            "b/x": {"input_cost_per_token": 2e-6, "output_cost_per_token": 1e-6},
        });
        let t = RateTable::parse(&doc);
        let tok = Tokens {
            uncached: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
            output: 1_000_000,
        };
        let p = t.price("claude-opus-5-5[1m]", 0, &tok).unwrap();
        assert!((p - 30.5).abs() < 1e-9, "{p}");
        let fast = t.price("claude-opus-5-5", 1, &tok).unwrap();
        assert!((fast - 61.0).abs() < 1e-9, "{fast}");
        // The same price under two prefixes is an alias; two prices are not.
        assert!(t.price("gpt-6-luna", 0, &tok).is_some());
        assert!(t.price("x", 0, &tok).is_none());
        assert!(t.price("opus", 0, &tok).is_none());
        // A missing cache-write price is plain input.
        let w = Tokens {
            cache_write: 1_000_000,
            ..Default::default()
        };
        assert!((t.price("gpt-6-luna", 0, &w).unwrap() - 0.1).abs() < 1e-12);
    }
}
