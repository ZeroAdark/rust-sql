//! `Kind` by name, and a consumer's two refusal texts with the kind in them (static, one per kind).

use super::Kind;

macro_rules! kinds {
    ($($k:ident),* $(,)?) => {
        impl Kind {
            pub const fn name(self) -> &'static str {
                match self {
                    $(Kind::$k => stringify!($k),)*
                }
            }

            pub const fn emit_refusal(self) -> &'static str {
                match self {
                    $(Kind::$k => concat!("an expression form the emitter does not serve [", stringify!($k), "]"),)*
                }
            }

            pub const fn compile_refusal(self) -> &'static str {
                match self {
                    $(Kind::$k => concat!("an expression node the compiler does not serve [", stringify!($k), "]"),)*
                }
            }
        }
    };
}

kinds!(
    Var,
    Const,
    Param,
    Aggref,
    GroupingFunc,
    WindowFunc,
    MergeSupportFunc,
    SubscriptingRef,
    FuncExpr,
    OpExpr,
    DistinctExpr,
    NullIfExpr,
    ScalarArrayOpExpr,
    BoolExpr,
    SubPlan,
    AlternativeSubPlan,
    FieldSelect,
    FieldStore,
    RelabelType,
    CoerceViaIO,
    ArrayCoerceExpr,
    ConvertRowtypeExpr,
    CollateExpr,
    CaseExpr,
    CaseTestExpr,
    ArrayExpr,
    RowExpr,
    RowCompareExpr,
    CoalesceExpr,
    MinMaxExpr,
    SQLValueFunction,
    XmlExpr,
    JsonValueExpr,
    JsonConstructorExpr,
    JsonIsPredicate,
    JsonExpr,
    NullTest,
    BooleanTest,
    CoerceToDomain,
    CoerceToDomainValue,
    SetToDefault,
    CurrentOfExpr,
    NextValueExpr,
    ReturningExpr,
    TargetEntry,
);
