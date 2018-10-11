use crate::error::{Error, ErrorKind};
use failure::ResultExt;
use rand::{
    distributions::{uniform::SampleUniform, Distribution, Uniform},
    Rng,
};
use regex_syntax::{
    hir::{self, Hir, HirKind},
    ParserBuilder,
};
use std::{
    char,
    collections::BTreeMap,
    fmt::Debug,
    iter,
    ops::{Add, AddAssign, Sub},
};

/// A compiled regex generator
#[derive(Clone, Debug)]
pub struct Generator {
    compiled: Compiled,
    capacity: usize,
}

#[derive(Clone, Debug)]
enum Compiled {
    Empty,
    Sequence(Vec<Compiled>),
    Literal(Vec<u8>),
    Repeat {
        count: Uniform<u32>,
        inner: Box<Compiled>,
        max_count: u32,
    },
    Any {
        index: Uniform<usize>,
        choices: Vec<Compiled>,
    },
    UnicodeClass {
        class: CompiledClass<u32>,
        max_char_len: usize,
    },
    ByteClass {
        class: CompiledClass<u8>,
    },
}

impl Compiled {
    fn eval_into(&self, rng: &mut impl Rng, output: &mut Vec<u8>) {
        match self {
            Compiled::Empty => {}
            Compiled::Sequence(seq) => {
                for elem in seq {
                    elem.eval_into(rng, output);
                }
            }
            Compiled::Literal(lit) => {
                output.extend_from_slice(lit);
            }
            Compiled::Repeat { count, inner, .. } => {
                let count = count.sample(rng);
                for _ in 0..count {
                    inner.eval_into(rng, output);
                }
            }
            Compiled::Any { index, choices } => {
                let index = index.sample(rng);
                choices[index].eval_into(rng, output);
            }
            Compiled::UnicodeClass { class, .. } => {
                let c = char::from_u32(class.sample(rng)).expect("valid char");
                let mut buf = [0; 4];
                output.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Compiled::ByteClass { class } => {
                let b = class.sample(rng);
                output.push(b);
            }
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Compiled::Empty => 0,
            Compiled::Sequence(seq) => seq.iter().map(Self::capacity).sum(),
            Compiled::Literal(lit) => lit.len(),
            Compiled::Repeat { inner, max_count, .. } => inner.capacity() * (*max_count as usize),
            Compiled::Any { choices, .. } => choices.iter().map(Self::capacity).max().unwrap_or(0),
            Compiled::UnicodeClass { max_char_len, .. } => *max_char_len,
            Compiled::ByteClass { .. } => 1,
        }
    }
}

fn simplify_sequence(mut seq: Vec<Compiled>) -> Compiled {
    let mut simplified = Vec::with_capacity(seq.len());
    seq.reverse();

    while let Some(elem) = seq.pop() {
        match elem {
            Compiled::Empty => continue,
            Compiled::Sequence(subseq) => {
                let sim = simplify_sequence(subseq);
                if let Compiled::Sequence(mut ss) = sim {
                    ss.reverse();
                    seq.append(&mut ss);
                } else {
                    seq.push(sim);
                }
            }
            Compiled::Literal(mut lit) => {
                if let Some(Compiled::Literal(prev_lit)) = simplified.last_mut() {
                    prev_lit.append(&mut lit);
                } else {
                    simplified.push(Compiled::Literal(lit));
                }
            }
            elem => simplified.push(elem),
        }
    }

    match simplified.len() {
        0 => Compiled::Empty,
        1 => simplified.swap_remove(0),
        _ => Compiled::Sequence(simplified),
    }
}

trait ClassRange {
    type Item: SampleUniform + Copy;
    const INVALID_RANGE: Option<(Self::Item, Self::Item)>;
    fn bounds(&self) -> (Self::Item, Self::Item);
}

impl ClassRange for hir::ClassUnicodeRange {
    type Item = u32;
    const INVALID_RANGE: Option<(Self::Item, Self::Item)> = Some((0xd7ff, 0xe000));
    fn bounds(&self) -> (Self::Item, Self::Item) {
        (self.start().into(), self.end().into())
    }
}

impl ClassRange for hir::ClassBytesRange {
    type Item = u8;
    const INVALID_RANGE: Option<(Self::Item, Self::Item)> = None;
    fn bounds(&self) -> (Self::Item, Self::Item) {
        (self.start(), self.end())
    }
}

#[derive(Clone, Debug)]
struct CompiledClass<T: SampleUniform>
where
    T::Sampler: Clone + Debug,
{
    searcher: Uniform<T>,
    ranges: BTreeMap<T, T>,
}

impl<T> Distribution<T> for CompiledClass<T>
where
    T: SampleUniform + Copy + Ord + Add<Output = T>,
    T::Sampler: Clone + Debug,
{
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> T {
        let normalized_index = self.searcher.sample(rng);
        let offset = self.ranges.range(..=normalized_index).next_back().expect("found").1;
        normalized_index + *offset
    }
}

fn compile_class<C>(ranges: &[C]) -> CompiledClass<C::Item>
where
    C: ClassRange,
    C::Item: From<u8> + Add<Output = C::Item> + Sub<Output = C::Item> + AddAssign + Copy + Ord,
    <C::Item as SampleUniform>::Sampler: Clone + Debug,
{
    let zero = C::Item::from(0);
    let one = C::Item::from(1);

    let mut normalized_ranges = BTreeMap::new();
    let mut normalized_len = zero;

    {
        let mut push = |start, end| {
            normalized_ranges.insert(normalized_len, start - normalized_len);
            normalized_len += end - start + one;
        };

        for r in ranges {
            let (start, end) = r.bounds();
            if let Some((invalid_start, invalid_end)) = C::INVALID_RANGE {
                if start <= invalid_start && invalid_end <= end {
                    push(start, invalid_start);
                    push(invalid_end, end);
                    continue;
                }
            }
            push(start, end);
        }
    }

    CompiledClass {
        searcher: Uniform::new(zero, normalized_len),
        ranges: normalized_ranges,
    }
}

fn compile_hir(hir: Hir, max_repeat: u32) -> Result<Compiled, Error> {
    Ok(match hir.into_kind() {
        HirKind::Empty => Compiled::Empty,
        HirKind::Anchor(anchor) => {
            let repr = Hir::anchor(anchor).to_string();
            return Err(ErrorKind::UnsupportedRegexElement(repr).into());
        }
        HirKind::WordBoundary(wb) => {
            let repr = Hir::word_boundary(wb).to_string();
            return Err(ErrorKind::UnsupportedRegexElement(repr).into());
        }
        HirKind::Literal(hir::Literal::Unicode(c)) => Compiled::Literal(c.to_string().into_bytes()),
        HirKind::Literal(hir::Literal::Byte(b)) => Compiled::Literal(vec![b]),
        HirKind::Class(hir::Class::Unicode(class)) => Compiled::UnicodeClass {
            class: compile_class(class.ranges()),
            max_char_len: class.iter().last().expect("at least 1 interval").end().len_utf8(),
        },
        HirKind::Class(hir::Class::Bytes(class)) => Compiled::ByteClass {
            class: compile_class(class.ranges()),
        },
        HirKind::Repetition(rep) => {
            let (lower, upper) = match rep.kind {
                hir::RepetitionKind::ZeroOrOne => (0, 1),
                hir::RepetitionKind::ZeroOrMore => (0, max_repeat),
                hir::RepetitionKind::OneOrMore => (1, 1 + max_repeat),
                hir::RepetitionKind::Range(range) => match range {
                    hir::RepetitionRange::Exactly(a) => (a, a),
                    hir::RepetitionRange::AtLeast(a) => (a, a + max_repeat),
                    hir::RepetitionRange::Bounded(a, b) => (a, b),
                },
            };
            let inner = compile_hir(*rep.hir, max_repeat)?;
            if lower == upper {
                match &inner {
                    Compiled::Empty => return Ok(Compiled::Empty),
                    Compiled::Literal(lit) => {
                        return Ok(if lower == 0 {
                            Compiled::Empty
                        } else {
                            // FIXME move to `slice::repeat` after #48784 is stabilized.
                            Compiled::Literal(
                                iter::repeat(lit.iter().cloned())
                                    .take(lower as usize)
                                    .flatten()
                                    .collect(),
                            )
                        });
                    }
                    _ => {}
                }
            }
            Compiled::Repeat {
                count: Uniform::new_inclusive(lower, upper),
                inner: Box::new(inner),
                max_count: upper,
            }
        }
        HirKind::Group(hir::Group { hir, .. }) => compile_hir(*hir, max_repeat)?,
        HirKind::Concat(hirs) => {
            let seq = hirs
                .into_iter()
                .map(|h| compile_hir(h, max_repeat))
                .collect::<Result<_, _>>()?;
            simplify_sequence(seq)
        }
        HirKind::Alternation(hirs) => {
            let mut choices = Vec::with_capacity(hirs.len());
            for hir in hirs {
                match compile_hir(hir, max_repeat)? {
                    Compiled::Any { choices: mut sc, .. } => choices.append(&mut sc),
                    compiled => choices.push(compiled),
                }
            }
            Compiled::Any {
                index: Uniform::new(0, choices.len()),
                choices,
            }
        }
    })
}

impl Generator {
    /// Compiles a regex pattern into a generator
    pub fn new(regex: &str, flags: &str, max_repeat: u32) -> Result<Self, Error> {
        let mut parser = ParserBuilder::new();
        for flag in flags.chars() {
            match flag {
                'o' => parser.octal(true),
                'a' => parser.allow_invalid_utf8(true).unicode(false),
                'u' => parser.allow_invalid_utf8(false).unicode(true),
                'x' => parser.ignore_whitespace(true),
                'i' => parser.case_insensitive(true),
                'm' => parser.multi_line(true),
                's' => parser.dot_matches_new_line(true),
                'U' => parser.swap_greed(true),
                _ => return Err(ErrorKind::UnknownRegexFlag(flag).into()),
            };
        }
        let hir = parser
            .build()
            .parse(regex)
            .with_context(|_| ErrorKind::InvalidRegex(regex.to_owned()))?;
        let compiled = compile_hir(hir, max_repeat)?;
        let capacity = compiled.capacity();
        Ok(Self { compiled, capacity })
    }

    /// Generates a new byte string which satisfies the regex pattern.
    pub fn eval(&self, rng: &mut impl Rng) -> Vec<u8> {
        let mut res = Vec::with_capacity(self.capacity);
        self.compiled.eval_into(rng, &mut res);
        res
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rand::{rngs::SmallRng, FromEntropy};
    use regex::Regex;
    use std::str::from_utf8;

    fn check(pattern: &str) {
        let r = Regex::new(pattern).unwrap();
        let gen = Generator::new(pattern, "", 100).unwrap();
        let mut rng = SmallRng::from_entropy();

        for _ in 0..10000 {
            let res = gen.eval(&mut rng);
            let s = from_utf8(&res).unwrap();
            assert!(r.is_match(s), "Wrong sample: {}", s);
        }
    }

    #[test]
    fn test_class() {
        check("[0-9A-Z]{24}");
        check(r"\d\D\s\S\w\W");
        check(".");
    }

    #[test]
    fn test_alt() {
        check("12{3,}|4{5,6}|7[89]");
    }
}
