// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

const MAX_INPUT_BYTES: usize = 1 << 20;
const MAX_FORMAT_WORK: usize = 1 << 20;
const MAX_OUTPUT_BYTES: usize = 16 << 20;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MaskedVal {
    pub val: u32,
    pub mask: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescEntry {
    pub idx: u32,
    pub size: u32,
    pub fixed: MaskedVal,
    pub name: Option<String>,
    pub flags: String,
}

pub type Desc = Vec<DescEntry>;

fn width_mask(size: u32) -> u32 {
    if size == 32 {
        u32::MAX
    } else {
        (1 << size) - 1
    }
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<(), String> {
    values
        .try_reserve(additional)
        .map_err(|_| "allocation failed".into())
}

fn charge(remaining: &mut usize, amount: u64) -> Result<(), String> {
    if amount > *remaining as u64 {
        return Err("format expansion work budget exceeded".into());
    }
    *remaining -= amount as usize;
    Ok(())
}

fn append(out: &mut String, text: &str) -> Result<(), String> {
    if text.len() > MAX_OUTPUT_BYTES.saturating_sub(out.len()) {
        return Err("format output byte budget exceeded".into());
    }
    out.try_reserve(text.len())
        .map_err(|_| "allocation failed".to_string())?;
    out.push_str(text);
    Ok(())
}

pub fn parse_desc(source: &str) -> Result<Desc, String> {
    if source.len() > MAX_INPUT_BYTES {
        return Err("descriptor input byte budget exceeded".into());
    }
    let mut entries = Vec::new();
    let mut used = 0u32;
    let mut offset = 0;
    while offset < source.len() {
        let tail = &source[offset..];
        let (size, bits, name, flags, consumed) = if matches!(tail.as_bytes()[0], b'0' | b'1') {
            let count = tail
                .bytes()
                .take_while(|b| matches!(b, b'0' | b'1'))
                .count();
            if count > 32 {
                return Err(format!("descriptor field exceeds 32 bits at byte {offset}"));
            }
            (count as u32, Some(&tail[..count]), None, "", count)
        } else if tail.starts_with('{') {
            let end = tail
                .find('}')
                .ok_or_else(|| format!("unterminated descriptor at byte {offset}"))?;
            let (size, rest) = tail[1..end]
                .split_once(':')
                .ok_or_else(|| format!("invalid descriptor at byte {offset}"))?;
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("invalid descriptor width at byte {offset}"));
            }
            let size = size
                .parse::<u32>()
                .map_err(|_| format!("invalid descriptor width at byte {offset}"))?;
            if !(1..=32).contains(&size) {
                return Err(format!(
                    "descriptor width must be in 1..=32 at byte {offset}"
                ));
            }
            let (name, bits) = match rest.split_once('=') {
                Some((name, bits)) => (name, Some(bits)),
                None => (rest, None),
            };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'|') {
                return Err(format!("invalid descriptor name at byte {offset}"));
            }
            if let Some(bits) = bits {
                if bits.len() != size as usize
                    || !bits.bytes().all(|b| matches!(b, b'0' | b'1' | b'x'))
                {
                    return Err(format!("invalid fixed descriptor bits at byte {offset}"));
                }
            }
            let (name, flags) = name.split_once('|').unwrap_or((name, ""));
            (size, bits, Some(name), flags, end + 1)
        } else {
            return Err(format!("invalid descriptor syntax at byte {offset}"));
        };
        if size > 32 - used {
            return Err(format!("descriptor exceeds 32 bits at byte {offset}"));
        }
        let mut fixed = MaskedVal::default();
        if let Some(bits) = bits {
            for byte in bits.bytes() {
                fixed.val = (fixed.val << 1) | u32::from(byte == b'1');
                fixed.mask = (fixed.mask << 1) | u32::from(byte != b'x');
            }
        }
        used += size;
        reserve(&mut entries, 1)?;
        entries.push(DescEntry {
            idx: 32 - used,
            size,
            fixed,
            name: name.map(str::to_owned),
            flags: flags.to_owned(),
        });
        offset += consumed;
    }
    if used != 32 {
        return Err(format!("descriptor contains {used} bits, not 32"));
    }
    Ok(entries)
}

#[derive(Clone, Debug)]
enum Component {
    Literal(String),
    Selection {
        mask: u32,
        values: Vec<(u32, usize)>,
        texts: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Fmt {
    fixed: MaskedVal,
    components: Vec<Component>,
}

#[derive(Clone, Copy)]
enum RuleMode {
    Insert,
    Overwrite,
    Delete,
}

struct MaskRange {
    fixed: MaskedVal,
    next: u64,
    end: u64,
}

impl MaskRange {
    fn new(fixed: MaskedVal, end: u64) -> Self {
        Self {
            fixed,
            next: u64::from(fixed.mask),
            end,
        }
    }
}

impl Iterator for MaskRange {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let value = (self.next as u32 & !self.fixed.mask) | self.fixed.val;
        self.next = (self.next + 1) | u64::from(self.fixed.mask);
        Some(value)
    }
}

fn parse_var(source: &str, desc: &Desc) -> Result<(u32, u32), String> {
    let (name, slice) = match source.split_once('<') {
        Some((name, slice)) => (
            name,
            Some(
                slice
                    .strip_suffix('>')
                    .ok_or_else(|| format!("invalid variable {source}"))?,
            ),
        ),
        None => (source, None),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(format!("invalid variable {source}"));
    }
    let field = desc
        .iter()
        .rev()
        .find(|entry| entry.name.as_deref() == Some(name))
        .ok_or_else(|| format!("unknown format variable {name}"))?;
    let Some(slice) = slice else {
        return Ok((field.idx, field.size));
    };
    let (high, low) = slice.split_once(':').unwrap_or((slice, slice));
    let parse_index = |text: &str| -> Result<u32, String> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid variable slice {source}"));
        }
        text.parse()
            .map_err(|_| format!("invalid variable slice {source}"))
    };
    let high = parse_index(high)?;
    let low = parse_index(low)?;
    if low > high || high >= 32 - field.idx {
        return Err(format!(
            "variable slice exceeds instruction width: {source}"
        ));
    }
    Ok((field.idx + low, high - low + 1))
}

fn parse_integer(source: &str) -> Result<u32, String> {
    let source = source.trim();
    let (negative, digits) = if let Some(digits) = source.strip_prefix('-') {
        (true, digits)
    } else {
        (false, source.strip_prefix('+').unwrap_or(source))
    };
    let (radix, digits, prefixed) = if digits.starts_with("0x") || digits.starts_with("0X") {
        (16, &digits[2..], true)
    } else if digits.starts_with("0b") || digits.starts_with("0B") {
        (2, &digits[2..], true)
    } else if digits.starts_with("0o") || digits.starts_with("0O") {
        (8, &digits[2..], true)
    } else {
        (10, digits, false)
    };
    let digits = if prefixed {
        digits.strip_prefix('_').unwrap_or(digits)
    } else {
        digits
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return Err(format!("invalid format integer {source}"));
    }
    let mut value = 0u32;
    for digit in digits.chars().filter(|c| *c != '_') {
        let digit = digit
            .to_digit(radix)
            .ok_or_else(|| format!("invalid format integer {source}"))?;
        value = value
            .checked_mul(radix)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(|| format!("format integer exceeds 32 bits: {source}"))?;
    }
    if (negative && value != 0) || (!prefixed && digits.starts_with('0') && value != 0) {
        return Err(format!("invalid nonnegative format integer {source}"));
    }
    Ok(value)
}

fn parse_selection(source: &str, desc: &Desc, remaining: &mut usize) -> Result<Component, String> {
    let mut in_slice = false;
    let separator = source
        .char_indices()
        .find_map(|(idx, c)| match c {
            '<' => {
                in_slice = true;
                None
            }
            '>' => {
                in_slice = false;
                None
            }
            ':' if !in_slice => Some(idx),
            _ => None,
        })
        .ok_or_else(|| "format selection has no clauses".to_string())?;
    let mut vars = Vec::new();
    let mut mask = 0u32;
    for variable in source[..separator].split(',') {
        let (shift, size) = parse_var(variable, desc)?;
        let variable_mask = width_mask(size) << shift;
        if mask & variable_mask != 0 {
            return Err("overlapping format variables".into());
        }
        mask |= variable_mask;
        reserve(&mut vars, 1)?;
        vars.push((shift, size));
    }
    let mut modes = Vec::new();
    let mut texts = Vec::new();
    let mut edits = Vec::new();
    for clause in source[separator + 1..].split(':') {
        let (mode, clause) = if let Some(clause) = clause.strip_prefix('!') {
            (RuleMode::Delete, clause)
        } else if let Some(clause) = clause.strip_prefix('^') {
            (RuleMode::Overwrite, clause)
        } else {
            (RuleMode::Insert, clause)
        };
        let (values, text) = if matches!(mode, RuleMode::Delete) {
            (
                clause.split_once('=').map_or(clause, |(values, _)| values),
                "",
            )
        } else {
            clause
                .split_once('=')
                .ok_or_else(|| "format clause has no assignment".to_string())?
        };
        if values.split(',').any(str::is_empty) {
            return Err("empty format clause value".into());
        }
        let mut wildcard = 0u32;
        let mut fixed = 0u32;
        let mut numvals = 0;
        // Extra values are ignored by the source grammar's zip, including in shipped descriptions.
        for (value, &(shift, size)) in values.split(',').zip(&vars) {
            if value == "*" {
                wildcard |= width_mask(size) << shift;
            } else {
                let value = parse_integer(value)?;
                if u64::from(value) >= 1u64 << size {
                    return Err("format clause value exceeds variable width".into());
                }
                fixed |= value << shift;
            }
            numvals += 1;
        }
        if numvals != vars.len() {
            return Err("too few values in format clause".into());
        }
        let count = 1u64 << wildcard.count_ones();
        charge(remaining, count)?;
        reserve(&mut edits, count as usize)?;
        reserve(&mut modes, 1)?;
        reserve(&mut texts, 1)?;
        let rule = modes.len();
        modes.push(mode);
        texts.push(text.to_owned());
        for key in MaskRange::new(
            MaskedVal {
                val: fixed,
                mask: !wildcard,
            },
            1u64 << 32,
        ) {
            edits.push((key, rule));
        }
    }
    edits.sort_unstable();
    let mut values = Vec::new();
    let mut key = None;
    let mut selected = None;
    for (edit_key, rule) in edits {
        if key != Some(edit_key) {
            if let (Some(key), Some(text)) = (key, selected) {
                reserve(&mut values, 1)?;
                values.push((key, text));
            }
            key = Some(edit_key);
            selected = None;
        }
        match modes[rule] {
            RuleMode::Insert if selected.is_none() => selected = Some(rule),
            RuleMode::Overwrite if selected.is_some() => selected = Some(rule),
            RuleMode::Delete if selected.is_some() => selected = None,
            RuleMode::Insert => return Err("duplicate format clause requires overwrite".into()),
            RuleMode::Overwrite => return Err("format overwrite has no existing value".into()),
            RuleMode::Delete => return Err("format negation has no existing value".into()),
        }
    }
    if let (Some(key), Some(text)) = (key, selected) {
        reserve(&mut values, 1)?;
        values.push((key, text));
    }
    Ok(Component::Selection {
        mask,
        values,
        texts,
    })
}

impl Fmt {
    pub fn parse(source: &str, desc: &Desc) -> Result<Self, String> {
        if source.len() > MAX_INPUT_BYTES {
            return Err("format input byte budget exceeded".into());
        }
        let mut fixed = MaskedVal::default();
        let mut occupied = 0u32;
        for entry in desc {
            if !(1..=32).contains(&entry.size) || entry.idx > 32 - entry.size {
                return Err("invalid descriptor field bounds".into());
            }
            let local_mask = width_mask(entry.size);
            let mask = local_mask << entry.idx;
            if occupied & mask != 0
                || entry.fixed.mask & !local_mask != 0
                || entry.fixed.val & !entry.fixed.mask != 0
            {
                return Err("invalid descriptor field mask".into());
            }
            occupied |= mask;
            fixed.val |= entry.fixed.val << entry.idx;
            fixed.mask |= entry.fixed.mask << entry.idx;
        }
        let mut components = Vec::new();
        let mut remaining = MAX_FORMAT_WORK;
        let mut offset = 0;
        while offset < source.len() {
            let tail = &source[offset..];
            let (component, consumed) = if tail.starts_with('{') {
                let end = tail
                    .find('}')
                    .ok_or_else(|| format!("unterminated format at byte {offset}"))?;
                (
                    parse_selection(&tail[1..end], desc, &mut remaining)?,
                    end + 1,
                )
            } else {
                let end = tail.find('{').unwrap_or(tail.len());
                (Component::Literal(tail[..end].to_owned()), end)
            };
            reserve(&mut components, 1)?;
            components.push(component);
            offset += consumed;
        }
        Ok(Self { fixed, components })
    }

    pub fn eval(&self, descvals: &[MaskedVal]) -> Result<Option<String>, String> {
        let mut remaining = MAX_FORMAT_WORK;
        self.eval_with_budget(descvals, &mut remaining)
    }

    fn eval_with_budget(
        &self,
        descvals: &[MaskedVal],
        remaining: &mut usize,
    ) -> Result<Option<String>, String> {
        let mut result = String::new();
        for component in &self.components {
            charge(remaining, 1)?;
            let (mask, values, texts) = match component {
                Component::Literal(text) => {
                    append(&mut result, text)?;
                    continue;
                }
                Component::Selection {
                    mask,
                    values,
                    texts,
                } => (*mask, values, texts),
            };
            let lookup = |key| {
                values
                    .binary_search_by_key(&key, |&(key, _)| key)
                    .ok()
                    .map(|idx| texts[values[idx].1].as_str())
            };
            let specialized = if descvals.len() == 1 && descvals[0].mask & mask == mask {
                lookup(descvals[0].val & mask)
            } else {
                let mut selected: Option<Option<&str>> = None;
                for value in descvals {
                    let fixed = MaskedVal {
                        val: value.val & mask,
                        mask: value.mask & mask,
                    };
                    // Gaps in this counter's mask participate in the upstream specialization check.
                    for key in MaskRange::new(fixed, u64::from(mask) + 1) {
                        charge(remaining, 1)?;
                        let candidate = lookup(key);
                        match selected {
                            None => selected = Some(candidate),
                            Some(previous) if previous == candidate => {}
                            Some(_) => {
                                return Err(
                                    "unable to specialize format: multiple candidates".into()
                                );
                            }
                        }
                    }
                }
                selected.ok_or_else(|| "unable to specialize format: no candidates".to_string())?
            };
            let Some(text) = specialized else {
                return Ok(None);
            };
            append(&mut result, text)?;
        }
        Ok(Some(result))
    }

    pub fn expansions(&self) -> Result<Vec<(String, Vec<MaskedVal>)>, String> {
        let mut specmask = 0u32;
        for component in &self.components {
            if let Component::Selection { mask, .. } = component {
                specmask |= mask;
            }
        }
        let count = 1u64 << specmask.count_ones();
        let mut remaining = MAX_FORMAT_WORK;
        charge(&mut remaining, count)?;
        let mask = self.fixed.mask | specmask;
        let mut expanded = Vec::new();
        let mut text_bytes = 0usize;
        for (ordinal, val) in MaskRange::new(
            MaskedVal {
                val: self.fixed.val,
                mask: !specmask,
            },
            1u64 << 32,
        )
        .enumerate()
        {
            let value = MaskedVal { val, mask };
            if let Some(name) = self.eval_with_budget(&[value], &mut remaining)? {
                if name.is_empty() {
                    continue;
                }
                if name.len() > MAX_OUTPUT_BYTES.saturating_sub(text_bytes) {
                    return Err("format expansion output byte budget exceeded".into());
                }
                text_bytes += name.len();
                reserve(&mut expanded, 1)?;
                expanded.push((name, value, ordinal));
            }
        }
        expanded.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        let mut groups: Vec<(String, Vec<MaskedVal>, usize)> = Vec::new();
        for (name, value, ordinal) in expanded {
            if let Some((previous, masks, _)) = groups.last_mut() {
                if *previous == name {
                    reserve(masks, 1)?;
                    masks.push(value);
                    continue;
                }
            }
            let mut masks = Vec::new();
            reserve(&mut masks, 1)?;
            masks.push(value);
            reserve(&mut groups, 1)?;
            groups.push((name, masks, ordinal));
        }
        groups.sort_unstable_by_key(|group| group.2);
        Ok(groups
            .into_iter()
            .map(|(name, values, _)| (name, values))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn low_desc() -> Desc {
        parse_desc("0000000000000000000000000000{2:a}{2:b}").unwrap()
    }

    #[test]
    fn descriptor_fields_fixed_bits_and_flags() {
        let desc = parse_desc("10{3:R|s=1x0}000000000000000000000000000").unwrap();
        assert_eq!(
            desc[0],
            DescEntry {
                idx: 30,
                size: 2,
                fixed: MaskedVal { val: 2, mask: 3 },
                name: None,
                flags: String::new()
            }
        );
        assert_eq!(
            desc[1],
            DescEntry {
                idx: 27,
                size: 3,
                fixed: MaskedVal { val: 4, mask: 5 },
                name: Some("R".into()),
                flags: "s".into()
            }
        );
        assert_eq!(parse_desc("{32:r|s|SP}").unwrap()[0].flags, "s|SP");
        for invalid in [
            "",
            "0",
            "{33:r}",
            "{0:r}",
            "{32:r=0}",
            "{32:r_name}",
            "{32:r} ",
            "{32:r}{1:s}",
            "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        ] {
            assert!(parse_desc(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn wildcards_negation_overwrite_and_insertion_order() {
        let fmt = Fmt::parse("{a,b:*,*=z:^1,2=a:!0,*:0,1=y}", &low_desc()).unwrap();
        let expanded = fmt.expansions().unwrap();
        assert_eq!(
            expanded.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
            ["y", "z", "a"]
        );
        assert_eq!(
            expanded[0].1,
            [MaskedVal {
                val: 1,
                mask: u32::MAX
            }]
        );
        assert_eq!(
            expanded[1].1.iter().map(|v| v.val).collect::<Vec<_>>(),
            [4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(expanded[2].1[0].val, 6);
    }

    #[test]
    fn duplicate_and_missing_edit_targets_fail() {
        for source in [
            "{a:0=x:0=y}",
            "{a:^0=x}",
            "{a:!0}",
            "{a:*=x:!0:!0}",
            "{a,b:0=x}",
            "{a,a:0,0=x}",
            "{a<30>:0=x}",
            "{a<0:1>:0=x}",
        ] {
            assert!(Fmt::parse(source, &low_desc()).is_err(), "{source}");
        }
    }

    #[test]
    fn extra_values_are_ignored_and_replacement_text_is_literal() {
        let fmt = Fmt::parse("{a,b:3,1,*=text,{=value}}tail", &low_desc()).unwrap();
        assert_eq!(
            fmt.eval(&[MaskedVal {
                val: 13,
                mask: u32::MAX
            }])
            .unwrap(),
            Some("text,{=value}tail".into())
        );
        assert!(Fmt::parse("{a:0,,ignored=x}", &low_desc()).is_err());
        let deleted = Fmt::parse("{b:0=x:!0=ignored}", &low_desc()).unwrap();
        assert!(deleted.expansions().unwrap().is_empty());
    }

    #[test]
    fn inclusive_slices_and_last_duplicate_name_win() {
        let desc = parse_desc("00000000000000000000000000{3:a}{3:a}").unwrap();
        let fmt = Fmt::parse("{a<2:1>:2=ok}", &desc).unwrap();
        assert_eq!(
            fmt.expansions().unwrap(),
            [(
                "ok".to_owned(),
                vec![MaskedVal {
                    val: 4,
                    mask: 0xffff_ffc6
                }]
            )]
        );
        let outside_field = Fmt::parse("{a<2>:0=outside}", &low_desc()).unwrap();
        assert_eq!(
            outside_field
                .eval(&[MaskedVal {
                    val: 0,
                    mask: u32::MAX
                }])
                .unwrap(),
            Some("outside".to_owned())
        );
    }

    #[test]
    fn evaluation_distinguishes_missing_empty_and_ambiguous() {
        let fmt = Fmt::parse("{b:0=:1=same:2=same}", &low_desc()).unwrap();
        assert_eq!(
            fmt.eval(&[MaskedVal { val: 0, mask: 3 }]).unwrap(),
            Some(String::new())
        );
        assert_eq!(fmt.eval(&[MaskedVal { val: 3, mask: 3 }]).unwrap(), None);
        assert_eq!(
            fmt.eval(&[MaskedVal { val: 1, mask: 3 }, MaskedVal { val: 2, mask: 3 }])
                .unwrap(),
            Some("same".into())
        );
        assert!(fmt.eval(&[MaskedVal { val: 2, mask: 2 }]).is_err());
        assert!(fmt.eval(&[]).is_err());
        assert_eq!(
            Fmt::parse("", &low_desc()).unwrap().eval(&[]).unwrap(),
            Some(String::new())
        );
        assert!(Fmt::parse("", &low_desc())
            .unwrap()
            .expansions()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn evaluation_counter_includes_gaps() {
        let fmt = Fmt::parse("{a<0>:*=same}", &low_desc()).unwrap();
        assert!(fmt.eval(&[MaskedVal::default()]).is_err());
        assert_eq!(
            fmt.eval(&[MaskedVal { val: 0, mask: 4 }]).unwrap(),
            Some("same".into())
        );
    }

    #[test]
    fn fixed_bits_do_not_filter_expansion_and_duplicates_survive() {
        let desc = parse_desc("000000000000000000000000000000{1:a=1}{1:b=0}").unwrap();
        let fmt = Fmt::parse("{a,b:*,*=same}", &desc).unwrap();
        assert_eq!(
            fmt.expansions().unwrap()[0]
                .1
                .iter()
                .map(|v| v.val)
                .collect::<Vec<_>>(),
            [2, 3, 2, 3]
        );
    }

    #[test]
    fn python_integer_bases_signs_and_separators() {
        for (text, expected) in [
            ("0x_f", 15),
            ("0b1_1", 3),
            ("+0o17", 15),
            ("0_0", 0),
            ("-0", 0),
            ("  +12  ", 12),
        ] {
            assert_eq!(parse_integer(text).unwrap(), expected);
        }
        for text in ["01", "-1", "1__0", "0x__f", "1_", "_1", "0x", "4294967296"] {
            assert!(parse_integer(text).is_err(), "{text}");
        }
    }

    #[test]
    fn expansion_and_evaluation_budgets_fail_before_unbounded_work() {
        let desc = parse_desc("{32:x}").unwrap();
        assert!(Fmt::parse("{x:*=too_many}", &desc)
            .unwrap_err()
            .contains("budget"));
        let fmt = Fmt::parse("{x:0=one}", &desc).unwrap();
        assert!(fmt.expansions().unwrap_err().contains("budget"));
        let missing = Fmt::parse("{x:0=one:!0}", &desc).unwrap();
        let mut remaining = 16;
        assert!(missing
            .eval_with_budget(&[MaskedVal::default()], &mut remaining)
            .unwrap_err()
            .contains("budget"));
        assert_eq!(remaining, 0);
        let sixteen_values = [MaskedVal {
            val: 0,
            mask: u32::MAX ^ 15,
        }];
        remaining = 16;
        assert!(missing
            .eval_with_budget(&sixteen_values, &mut remaining)
            .unwrap_err()
            .contains("budget"));
        assert_eq!(remaining, 0);
        remaining = 17;
        assert_eq!(
            missing
                .eval_with_budget(&sixteen_values, &mut remaining)
                .unwrap(),
            None
        );
        assert_eq!(remaining, 0);
        assert_eq!(missing.eval(&sixteen_values).unwrap(), None);
        assert_eq!(
            fmt.eval(&[MaskedVal {
                val: 0,
                mask: u32::MAX
            }])
            .unwrap(),
            Some("one".into())
        );
    }
}
