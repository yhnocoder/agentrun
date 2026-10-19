use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct TokenCounts {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl TokenCounts {
    pub fn context_tokens(&self) -> Option<u64> {
        Some(self.input_tokens? + self.cache_read_tokens? + self.cache_write_tokens?)
    }

    pub fn summary(&self, with_output: bool) -> Vec<String> {
        let input = [
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        ]
        .into_iter()
        .flatten()
        .reduce(|a, b| a + b);
        let mut items = Vec::new();
        if let Some(input) = input {
            items.push(format!("in {}", tokens(input)));
        }
        if let Some(output) = self.output_tokens.filter(|_| with_output) {
            items.push(format!("out {}", tokens(output)));
        }
        if let (Some(read), Some(input)) = (self.cache_read_tokens, input.filter(|n| *n > 0)) {
            let percent = (read as f64 / input as f64 * 100.0).round() as u64;
            items.push(format!("cached {percent}%"));
        }
        items
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.input_tokens = add(self.input_tokens, other.input_tokens);
        self.output_tokens = add(self.output_tokens, other.output_tokens);
        self.cache_read_tokens = add(self.cache_read_tokens, other.cache_read_tokens);
        self.cache_write_tokens = add(self.cache_write_tokens, other.cache_write_tokens);
        self.cost_usd = add(self.cost_usd, other.cost_usd);
    }
}

pub fn cost(usd: f64) -> String {
    format!("${usd:.4}")
}

pub fn tokens(count: u64) -> String {
    if count < 100 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

fn add<T: Copy + Default + std::ops::Add<Output = T>>(
    total: Option<T>,
    value: Option<T>,
) -> Option<T> {
    match (total, value) {
        (None, None) => None,
        _ => Some(total.unwrap_or_default() + value.unwrap_or_default()),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Usage {
    #[serde(flatten)]
    pub totals: TokenCounts,
    pub by_model: BTreeMap<String, TokenCounts>,
}

impl Usage {
    pub fn sum<'a>(items: impl IntoIterator<Item = (Option<&'a str>, &'a TokenCounts)>) -> Usage {
        let mut usage = Usage::default();
        for (model, counts) in items {
            usage.totals.add(counts);
            usage
                .by_model
                .entry(model.unwrap_or("unknown").to_string())
                .or_default()
                .add(counts);
        }
        usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(
        input: Option<u64>,
        output: Option<u64>,
        read: Option<u64>,
        write: Option<u64>,
    ) -> TokenCounts {
        TokenCounts {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
            cost_usd: None,
        }
    }

    #[test]
    fn context_tokens_needs_all_three_inputs() {
        assert_eq!(
            counts(Some(1), Some(9), Some(2), Some(3)).context_tokens(),
            Some(6)
        );
        assert_eq!(
            counts(Some(1), Some(9), None, Some(3)).context_tokens(),
            None
        );
        assert_eq!(
            counts(Some(1), None, Some(2), Some(0)).context_tokens(),
            Some(3)
        );
    }

    #[test]
    fn summary_items_follow_the_display_rules() {
        assert_eq!(
            counts(Some(18234), Some(912), Some(42000), Some(5100)).summary(true),
            vec!["in 65.3k", "out 0.9k", "cached 64%"]
        );
        assert_eq!(
            counts(Some(18234), Some(912), Some(42000), Some(5100)).summary(false),
            vec!["in 65.3k", "cached 64%"]
        );
        assert_eq!(
            counts(None, Some(3), None, Some(4)).summary(true),
            vec!["in 4", "out 3"]
        );
        assert_eq!(
            counts(Some(0), None, Some(0), None).summary(true),
            vec!["in 0"]
        );
        assert!(TokenCounts::default().summary(true).is_empty());
        assert_eq!(tokens(99), "99");
        assert_eq!(tokens(100), "0.1k");
        assert_eq!(tokens(1_234_567), "1.2M");
    }

    #[test]
    fn sum_of_nothing_is_null_with_empty_by_model() {
        let usage = Usage::sum([]);
        assert_eq!(usage.totals, TokenCounts::default());
        assert!(usage.by_model.is_empty());
        assert_eq!(
            serde_json::to_string(&usage).unwrap(),
            r#"{"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"cost_usd":null,"by_model":{}}"#
        );
    }

    #[test]
    fn sum_adds_only_non_null_values() {
        let a = counts(Some(10), None, Some(5), None);
        let b = counts(Some(1), None, None, None);
        let c = counts(None, None, None, None);
        let usage = Usage::sum([(Some("m1"), &a), (None, &b), (Some("m1"), &c)]);
        assert_eq!(usage.totals, counts(Some(11), None, Some(5), None));
        assert_eq!(usage.by_model["m1"], counts(Some(10), None, Some(5), None));
        assert_eq!(usage.by_model["unknown"], counts(Some(1), None, None, None));
    }

    #[test]
    fn cost_follows_the_same_null_rule() {
        let priced = |usd| TokenCounts {
            cost_usd: usd,
            ..TokenCounts::default()
        };
        let usage = Usage::sum([
            (Some("m1"), &priced(Some(0.25))),
            (Some("m1"), &priced(None)),
            (Some("m2"), &priced(None)),
            (Some("m3"), &priced(Some(0.0))),
            (Some("m1"), &priced(Some(0.5))),
        ]);
        assert_eq!(usage.totals.cost_usd, Some(0.75));
        assert_eq!(usage.by_model["m1"].cost_usd, Some(0.75));
        assert_eq!(usage.by_model["m2"].cost_usd, None);
        assert_eq!(usage.by_model["m3"].cost_usd, Some(0.0));
        assert_eq!(cost(0.0123456), "$0.0123");
        assert_eq!(cost(0.0), "$0.0000");
        assert_eq!(cost(12.34567), "$12.3457");
    }
}
