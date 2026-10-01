//! Dictionary types: their wire sizes, and literal arguments serialised to them.
//!
//! Serialisation is F Prime's: big-endian, a string as its length (`FwSizeStoreType`) then
//! only the bytes it holds, a bool as `FW_SERIALIZE_TRUE_VALUE` or `FW_SERIALIZE_FALSE_VALUE`,
//! an enum as its representation type, arrays and structs as their elements in order.

use super::diag::Diagnostic;
use super::parse::{Value, ValueKind};
use fprime_dictionary::{
    Dictionary, EnumType, FloatKind, IntegerKind, StructType, TypeDefinition, TypeName,
};

/// Arrays and structs nested deeper than this are taken to be a cyclic dictionary.
const MAX_TYPE_DEPTH: usize = 32;

/// A dictionary type, aliases followed.
#[derive(Debug, Clone, Copy)]
pub enum Shape<'d> {
    Int(IntegerKind),
    Float(FloatKind),
    Bool,
    Str { capacity: u32 },
    Enum(&'d EnumType),
    Array { element: &'d TypeName, size: u32 },
    Struct(&'d StructType),
}

/// How one deployment puts values on the wire.
pub struct Wire<'d> {
    pub dict: &'d Dictionary,
    /// `FwSizeStoreType`, the length prefix on a string. `U16` unless the dictionary says.
    size_store: IntegerKind,
    pub true_value: u8,
    pub false_value: u8,
}

impl<'d> Wire<'d> {
    pub fn new(dict: &'d Dictionary) -> Self {
        let size_store = match dict.resolved_type(&TypeName::QualifiedIdentifier {
            name: "FwSizeStoreType".into(),
        }) {
            Some(TypeName::Integer { name }) => *name,
            _ => IntegerKind::U16,
        };
        Wire {
            dict,
            size_store,
            true_value: byte_constant(dict, "FW_SERIALIZE_TRUE_VALUE").unwrap_or(0xFF),
            false_value: byte_constant(dict, "FW_SERIALIZE_FALSE_VALUE").unwrap_or(0x00),
        }
    }

    pub fn shape(&self, ty: &'d TypeName) -> Result<Shape<'d>, String> {
        let unknown = || format!("the dictionary does not define the type {}", describe(ty));
        Ok(match self.dict.resolved_type(ty).ok_or_else(unknown)? {
            TypeName::Integer { name } => Shape::Int(*name),
            TypeName::Float { name } => Shape::Float(*name),
            TypeName::Bool => Shape::Bool,
            TypeName::String { size } => Shape::Str { capacity: *size },
            TypeName::QualifiedIdentifier { name } => {
                match self.dict.type_definitions.get(name).ok_or_else(unknown)? {
                    TypeDefinition::Enum(ty) => Shape::Enum(ty),
                    TypeDefinition::Array(ty) => Shape::Array {
                        element: &ty.element_type,
                        size: ty.size,
                    },
                    TypeDefinition::Struct(ty) => Shape::Struct(ty),
                    // `resolved_type` follows aliases to their end, or gives up.
                    TypeDefinition::Alias(_) => return Err(unknown()),
                }
            }
        })
    }

    /// The integer an enum is serialised as.
    pub fn representation(&self, ty: &'d EnumType) -> Result<IntegerKind, String> {
        match self.shape(&ty.representation_type)? {
            Shape::Int(kind) => Ok(kind),
            _ => Err(format!(
                "{} is represented by a type that is not an integer",
                ty.qualified_name
            )),
        }
    }

    /// The most bytes a value of `ty` serialises to.
    pub fn max_size(&self, ty: &'d TypeName) -> Result<u32, String> {
        self.size(ty, false, 0)
            .map(|size| size.expect("every type has a largest size"))
    }

    /// The bytes every value of `ty` serialises to, or `None` if that depends on the value,
    /// because a string is in it somewhere.
    pub fn fixed_size(&self, ty: &'d TypeName) -> Result<Option<u32>, String> {
        self.size(ty, true, 0)
    }

    fn size(&self, ty: &'d TypeName, fixed: bool, depth: usize) -> Result<Option<u32>, String> {
        if depth > MAX_TYPE_DEPTH {
            return Err(format!(
                "{} nests too deeply to be serialised",
                describe(ty)
            ));
        }
        let too_large = || format!("{} is too large to serialise", describe(ty));
        let size = match self.shape(ty)? {
            Shape::Int(kind) => int_size(kind),
            Shape::Float(FloatKind::F32) => 4,
            Shape::Float(FloatKind::F64) => 8,
            Shape::Bool => 1,
            Shape::Str { capacity } => {
                if fixed {
                    return Ok(None);
                }
                int_size(self.size_store)
                    .checked_add(capacity)
                    .ok_or_else(too_large)?
            }
            Shape::Enum(ty) => int_size(self.representation(ty)?),
            Shape::Array { element, size } => {
                let Some(element) = self.size(element, fixed, depth + 1)? else {
                    return Ok(None);
                };
                element.checked_mul(size).ok_or_else(too_large)?
            }
            Shape::Struct(ty) => {
                let mut total = 0u32;
                for member in &ty.members {
                    let Some(one) = self.size(&member.type_name, fixed, depth + 1)? else {
                        return Ok(None);
                    };
                    total = one
                        .checked_mul(member.size.unwrap_or(1))
                        .and_then(|member| total.checked_add(member))
                        .ok_or_else(too_large)?;
                }
                total
            }
        };
        Ok(Some(size))
    }

    /// Serialise `value` as a `ty`. `what` names it in an error: `arg1`, `choices[0].first`.
    pub fn encode(
        &self,
        ty: &'d TypeName,
        value: &Value,
        what: &str,
        out: &mut Vec<u8>,
    ) -> Result<(), Diagnostic> {
        self.encode_at(ty, value, what, out, 0)
    }

    fn encode_at(
        &self,
        ty: &'d TypeName,
        value: &Value,
        what: &str,
        out: &mut Vec<u8>,
        depth: usize,
    ) -> Result<(), Diagnostic> {
        let fail = |message: String| Diagnostic::error(value.span, message);
        let mismatch = || {
            fail(format!(
                "`{what}` is {}, not {}",
                describe(ty),
                value.kind.describe()
            ))
        };
        if depth > MAX_TYPE_DEPTH {
            return Err(fail(format!(
                "{} nests too deeply to be serialised",
                describe(ty)
            )));
        }

        match (self.shape(ty).map_err(fail)?, &value.kind) {
            (Shape::Int(kind), ValueKind::Int(number)) => {
                out.extend(integer(kind, *number).ok_or_else(|| {
                    fail(format!(
                        "`{what}` is {}, which cannot hold {number} ({})",
                        describe(ty),
                        range_text(kind)
                    ))
                })?);
            }
            (Shape::Float(kind), ValueKind::Int(_) | ValueKind::Float(_)) => {
                let number = match value.kind {
                    ValueKind::Int(number) => number as f64,
                    ValueKind::Float(number) => number,
                    _ => unreachable!("matched above"),
                };
                match kind {
                    FloatKind::F32 => {
                        let single = number as f32;
                        if single.is_infinite() {
                            return Err(fail(format!(
                                "`{what}` is {}, which cannot hold {number}",
                                describe(ty)
                            )));
                        }
                        out.extend(single.to_be_bytes());
                    }
                    FloatKind::F64 => out.extend(number.to_be_bytes()),
                }
            }
            (Shape::Bool, ValueKind::Bool(flag)) => {
                out.push(if *flag {
                    self.true_value
                } else {
                    self.false_value
                });
            }
            (Shape::Str { capacity }, ValueKind::Str(text)) => {
                let bytes = text.as_bytes();
                if bytes.len() > capacity as usize {
                    return Err(fail(format!(
                        "`{what}` is {}, too small for this {}-byte string",
                        describe(ty),
                        bytes.len()
                    )));
                }
                let length = integer(self.size_store, bytes.len() as i128).ok_or_else(|| {
                    fail(format!(
                        "this {}-byte string is too long for its {:?} length prefix",
                        bytes.len(),
                        self.size_store
                    ))
                })?;
                out.extend(length);
                out.extend(bytes);
            }
            (Shape::Enum(enumeration), ValueKind::Name(name)) => {
                let Some(constant) = enum_constant(enumeration, name) else {
                    return Err(fail(format!(
                        "`{what}` is {}, which has no constant `{name}`; it has {}",
                        enumeration.qualified_name,
                        constants(enumeration)
                    )));
                };
                let kind = self.representation(enumeration).map_err(fail)?;
                out.extend(integer(kind, i128::from(constant)).ok_or_else(|| {
                    fail(format!(
                        "{}.{name} does not fit its representation type",
                        enumeration.qualified_name
                    ))
                })?);
            }
            (Shape::Enum(enumeration), _) => {
                return Err(fail(format!(
                    "`{what}` is {}, an enum: name one of its constants ({}), not {}",
                    enumeration.qualified_name,
                    constants(enumeration),
                    value.kind.describe()
                )));
            }
            (Shape::Array { element, size }, ValueKind::Array(elements)) => {
                if elements.len() != size as usize {
                    return Err(fail(format!(
                        "`{what}` is {}, an array of {size}, but {} element{} given",
                        describe(ty),
                        elements.len(),
                        if elements.len() == 1 {
                            " was"
                        } else {
                            "s were"
                        }
                    )));
                }
                for (index, element_value) in elements.iter().enumerate() {
                    let what = format!("{what}[{index}]");
                    self.encode_at(element, element_value, &what, out, depth + 1)?;
                }
            }
            (Shape::Struct(structure), ValueKind::Struct(members)) => {
                for given in members {
                    if !structure.members.iter().any(|m| m.name == given.name) {
                        return Err(Diagnostic::error(
                            given.span,
                            format!(
                                "`{what}` is {}, which has no member `{}`; its members are {}",
                                structure.qualified_name,
                                given.name,
                                member_names(structure)
                            ),
                        ));
                    }
                    if members.iter().filter(|m| m.name == given.name).count() > 1 {
                        return Err(Diagnostic::error(
                            given.span,
                            format!("member `{}` is given more than once", given.name),
                        ));
                    }
                }
                // In declaration order, which is the order they go on the wire.
                for member in &structure.members {
                    let Some(given) = members.iter().find(|m| m.name == member.name) else {
                        return Err(fail(format!(
                            "`{what}` is {}, and member `{}` is missing",
                            structure.qualified_name, member.name
                        )));
                    };
                    let what = format!("{what}.{}", member.name);
                    match member.size {
                        None => {
                            self.encode_at(&member.type_name, &given.value, &what, out, depth + 1)?
                        }
                        Some(size) => {
                            let ValueKind::Array(elements) = &given.value.kind else {
                                return Err(Diagnostic::error(
                                    given.value.span,
                                    format!(
                                        "`{what}` is an array of {size} {}, not {}",
                                        describe(&member.type_name),
                                        given.value.kind.describe()
                                    ),
                                ));
                            };
                            if elements.len() != size as usize {
                                return Err(Diagnostic::error(
                                    given.value.span,
                                    format!(
                                        "`{what}` is an array of {size}, but {} given",
                                        elements.len()
                                    ),
                                ));
                            }
                            for (index, element) in elements.iter().enumerate() {
                                let what = format!("{what}[{index}]");
                                self.encode_at(&member.type_name, element, &what, out, depth + 1)?;
                            }
                        }
                    }
                }
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }
}

/// A dictionary constant that names a byte, e.g. `FW_SERIALIZE_TRUE_VALUE`, at the top level
/// or in a module.
fn byte_constant(dict: &Dictionary, name: &str) -> Option<u8> {
    let suffix = format!(".{name}");
    dict.constants
        .iter()
        .find(|c| c.qualified_name == name || c.qualified_name.ends_with(&suffix))
        .and_then(|c| match c.value {
            fprime_dictionary::Value::Integer(value) => u8::try_from(value).ok(),
            _ => None,
        })
}

/// An enum's constant, named bare (`ONE`) or qualified (`Ref.Choice.ONE`).
pub fn enum_constant(enumeration: &EnumType, name: &str) -> Option<i64> {
    let bare = name
        .strip_prefix(enumeration.qualified_name.as_str())
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(name);
    enumeration
        .enumerated_constants
        .iter()
        .find(|constant| constant.name == bare)
        .map(|constant| constant.value)
}

/// `ONE, TWO, RED, BLUE`
pub fn constants(enumeration: &EnumType) -> String {
    enumeration
        .enumerated_constants
        .iter()
        .map(|constant| constant.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn member_names(structure: &StructType) -> String {
    structure
        .members
        .iter()
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// How a type is written in FPP: `U32`, `string size 40`, `Ref.Choice`.
pub fn describe(ty: &TypeName) -> String {
    match ty {
        TypeName::Integer { name } => format!("{name:?}"),
        TypeName::Float { name } => format!("{name:?}"),
        TypeName::Bool => "bool".into(),
        TypeName::String { size } => format!("string size {size}"),
        TypeName::QualifiedIdentifier { name } => name.clone(),
    }
}

pub fn int_size(kind: IntegerKind) -> u32 {
    match kind {
        IntegerKind::U8 | IntegerKind::I8 => 1,
        IntegerKind::U16 | IntegerKind::I16 => 2,
        IntegerKind::U32 | IntegerKind::I32 => 4,
        IntegerKind::U64 | IntegerKind::I64 => 8,
    }
}

pub fn signed(kind: IntegerKind) -> bool {
    matches!(
        kind,
        IntegerKind::I8 | IntegerKind::I16 | IntegerKind::I32 | IntegerKind::I64
    )
}

/// The smallest and largest value of `kind`.
pub fn range(kind: IntegerKind) -> (i128, i128) {
    let bits = int_size(kind) * 8;
    if signed(kind) {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

pub fn range_text(kind: IntegerKind) -> String {
    let (low, high) = range(kind);
    format!("{low} to {high}")
}

/// `value` big-endian in `kind`'s width, if it fits.
pub fn integer(kind: IntegerKind, value: i128) -> Option<Vec<u8>> {
    let (low, high) = range(kind);
    if value < low || value > high {
        return None;
    }
    let width = int_size(kind) as usize;
    // Two's complement in 16 bytes; the low `width` are the value at `kind`'s width.
    Some(value.to_be_bytes()[16 - width..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seq::diag::Span;
    use crate::seq::parse::Member;
    use std::path::Path;

    fn dictionary() -> Dictionary {
        fprime_dictionary::parse(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fprime_dictionary/src/test/RefTopologyDictionary.json"
        )))
    }

    fn value(kind: ValueKind) -> Value {
        Value {
            span: Span::new(1, 1),
            kind,
        }
    }

    fn named(name: &str) -> TypeName {
        TypeName::QualifiedIdentifier { name: name.into() }
    }

    fn encode(dict: &Dictionary, ty: &TypeName, kind: ValueKind) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        Wire::new(dict)
            .encode(ty, &value(kind), "arg", &mut out)
            .map(|()| out)
            .map_err(|diagnostic| diagnostic.message)
    }

    #[test]
    fn integers_are_big_endian_and_range_checked() {
        assert_eq!(integer(IntegerKind::U8, 255), Some(vec![0xFF]));
        assert_eq!(integer(IntegerKind::U8, 256), None);
        assert_eq!(integer(IntegerKind::I8, -1), Some(vec![0xFF]));
        assert_eq!(integer(IntegerKind::I8, -129), None);
        assert_eq!(integer(IntegerKind::U16, 0x1234), Some(vec![0x12, 0x34]));
        assert_eq!(
            integer(IntegerKind::I32, -2),
            Some(vec![0xFF, 0xFF, 0xFF, 0xFE])
        );
        assert_eq!(integer(IntegerKind::U32, -1), None);
        assert_eq!(
            integer(IntegerKind::U64, u64::MAX as i128),
            Some(vec![0xFF; 8])
        );
        assert_eq!(integer(IntegerKind::I64, i64::MIN as i128 - 1), None);
        assert_eq!(range_text(IntegerKind::I16), "-32768 to 32767");
    }

    #[test]
    fn scalars() {
        let dict = dictionary();
        let u32_type = TypeName::Integer {
            name: IntegerKind::U32,
        };
        assert_eq!(
            encode(&dict, &u32_type, ValueKind::Int(0x01020304)),
            Ok(vec![1, 2, 3, 4])
        );
        assert_eq!(
            encode(&dict, &named("Ref.AliasedU32"), ValueKind::Int(7)),
            Ok(vec![0, 0, 0, 7]),
            "an alias encodes as what it aliases"
        );
        assert_eq!(
            encode(
                &dict,
                &TypeName::Float {
                    name: FloatKind::F32
                },
                ValueKind::Float(1.5)
            ),
            Ok(1.5f32.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode(
                &dict,
                &TypeName::Float {
                    name: FloatKind::F64
                },
                ValueKind::Int(-2)
            ),
            Ok((-2.0f64).to_be_bytes().to_vec()),
            "an integer is accepted for a float"
        );
        assert_eq!(
            encode(&dict, &TypeName::Bool, ValueKind::Bool(true)),
            Ok(vec![0xFF]),
            "FW_SERIALIZE_TRUE_VALUE"
        );
        assert_eq!(
            encode(&dict, &TypeName::Bool, ValueKind::Bool(false)),
            Ok(vec![0x00])
        );
    }

    #[test]
    fn strings_carry_their_length_and_nothing_more() {
        let dict = dictionary();
        let ty = TypeName::String { size: 8 };
        assert_eq!(
            encode(&dict, &ty, ValueKind::Str("hi".into())),
            Ok(vec![0, 2, b'h', b'i'])
        );
        assert_eq!(
            encode(&dict, &ty, ValueKind::Str("12345678".into())),
            Ok([&[0u8, 8][..], b"12345678"].concat())
        );
        let err = encode(&dict, &ty, ValueKind::Str("123456789".into())).unwrap_err();
        assert_eq!(
            err,
            "`arg` is string size 8, too small for this 9-byte string"
        );
    }

    #[test]
    fn enums_by_bare_or_qualified_name() {
        let dict = dictionary();
        assert_eq!(
            encode(&dict, &named("Ref.Choice"), ValueKind::Name("BLUE".into())),
            Ok(vec![0, 0, 0, 3])
        );
        assert_eq!(
            encode(
                &dict,
                &named("Ref.Choice"),
                ValueKind::Name("Ref.Choice.RED".into())
            ),
            Ok(vec![0, 0, 0, 2])
        );
        assert_eq!(
            encode(
                &dict,
                &named("Ref.WideChoice"),
                ValueKind::Name("WIDE_LOW".into())
            ),
            Ok((-4294967296i64).to_be_bytes().to_vec())
        );
        assert_eq!(
            encode(
                &dict,
                &named("Ref.Choice"),
                ValueKind::Name("PURPLE".into())
            )
            .unwrap_err(),
            "`arg` is Ref.Choice, which has no constant `PURPLE`; it has ONE, TWO, RED, BLUE"
        );
        assert!(
            encode(&dict, &named("Ref.Choice"), ValueKind::Int(1))
                .unwrap_err()
                .contains("an enum: name one of its constants")
        );
    }

    #[test]
    fn arrays_and_structs() {
        let dict = dictionary();
        let ones = |n| {
            (0..n)
                .map(|_| value(ValueKind::Name("TWO".into())))
                .collect()
        };
        assert_eq!(
            encode(&dict, &named("Ref.ManyChoices"), ValueKind::Array(ones(2))),
            Ok(vec![0, 0, 0, 1, 0, 0, 0, 1])
        );
        assert_eq!(
            encode(&dict, &named("Ref.ManyChoices"), ValueKind::Array(ones(3))).unwrap_err(),
            "`arg` is Ref.ManyChoices, an array of 2, but 3 elements were given"
        );

        let member = |name: &str, constant: &str| Member {
            name: name.into(),
            span: Span::new(1, 1),
            value: value(ValueKind::Name(constant.into())),
        };
        // Given out of order; serialised in declaration order.
        assert_eq!(
            encode(
                &dict,
                &named("Ref.ChoicePair"),
                ValueKind::Struct(vec![
                    member("secondChoice", "BLUE"),
                    member("firstChoice", "TWO")
                ])
            ),
            Ok(vec![0, 0, 0, 1, 0, 0, 0, 3])
        );
        assert_eq!(
            encode(
                &dict,
                &named("Ref.ChoicePair"),
                ValueKind::Struct(vec![member("firstChoice", "TWO")])
            )
            .unwrap_err(),
            "`arg` is Ref.ChoicePair, and member `secondChoice` is missing"
        );
        assert_eq!(
            encode(
                &dict,
                &named("Ref.ChoicePair"),
                ValueKind::Struct(vec![member("third", "TWO")])
            )
            .unwrap_err(),
            "`arg` is Ref.ChoicePair, which has no member `third`; its members are \
             firstChoice, secondChoice"
        );
    }

    #[test]
    fn strings_inside_arrays_encode_too() {
        // A literal is serialised here, at compile time, so a nested string costs nothing
        // extra: it is not the case the Rust const encoder has to refuse.
        let dict = dictionary();
        let names = vec![
            value(ValueKind::Str("a".into())),
            value(ValueKind::Str("bc".into())),
        ];
        assert_eq!(
            encode(
                &dict,
                &named("Ref.DpDemo.StringArray"),
                ValueKind::Array(names)
            ),
            Ok(vec![0, 1, b'a', 0, 2, b'b', b'c'])
        );
    }

    #[test]
    fn mismatches_name_the_type() {
        let dict = dictionary();
        assert_eq!(
            encode(&dict, &TypeName::String { size: 40 }, ValueKind::Int(3)).unwrap_err(),
            "`arg` is string size 40, not `3`"
        );
        assert_eq!(
            encode(
                &dict,
                &TypeName::Integer {
                    name: IntegerKind::U8
                },
                ValueKind::Int(300)
            )
            .unwrap_err(),
            "`arg` is U8, which cannot hold 300 (0 to 255)"
        );
        assert_eq!(
            encode(
                &dict,
                &TypeName::Integer {
                    name: IntegerKind::U8
                },
                ValueKind::Float(1.5)
            )
            .unwrap_err(),
            "`arg` is U8, not `1.5`"
        );
        assert!(
            encode(
                &dict,
                &TypeName::Float {
                    name: FloatKind::F32
                },
                ValueKind::Float(1e300)
            )
            .unwrap_err()
            .contains("cannot hold")
        );
    }

    #[test]
    fn sizes() {
        let dict = dictionary();
        let wire = Wire::new(&dict);
        let scalar_struct = named("Ref.ScalarStruct");
        assert_eq!(
            wire.fixed_size(&scalar_struct),
            Ok(Some(1 + 2 + 4 + 8 + 1 + 2 + 4 + 8 + 4 + 8))
        );
        let string_array = named("Ref.DpDemo.StringArray");
        assert_eq!(wire.fixed_size(&string_array), Ok(None));
        assert_eq!(wire.max_size(&string_array), Ok(2 * (2 + 80)));
        assert_eq!(wire.max_size(&named("Ref.WideChoice")), Ok(8));
        assert_eq!(wire.max_size(&named("FwSizeType")), Ok(8));
        assert!(wire.max_size(&named("Nope")).is_err());
    }
}
