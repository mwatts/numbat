use std::collections::HashMap;

use crate::arithmetic::{Exponent, Power, Rational};
use crate::ast;
use crate::dimension::DimensionRegistry;
use crate::registry::{BaseRepresentation, RegistryError};
use crate::typed_ast::{self, Type};

use num_traits::FromPrimitive;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TypeCheckError {
    #[error("Unknown identifier '{0}'")]
    UnknownIdentifier(String),

    #[error("Unknown function '{0}'")]
    UnknownFunction(String),

    #[error("Incompatible dimensions in {0}:\n    {1}: {2}\n    {3}: {4}")]
    IncompatibleDimensions(
        String,
        &'static str,
        BaseRepresentation,
        &'static str,
        BaseRepresentation,
    ),

    #[error("Got dimension {0}, but exponent must be dimensionless.")]
    NonScalarExponent(BaseRepresentation),

    #[error("{0}")]
    RegistryError(RegistryError),

    #[error("Incompatible alternative expressions have been provided for dimension '{0}'")]
    IncompatibleAlternativeDimensionExpression(String),

    #[error("Function '{0}' called with {2} arguments(s), but needs {1}.")]
    WrongArity(String, usize, usize),

    #[error("Unsupported expression in const-evaluation of exponent: {0}")]
    UnsupportedConstExpression(&'static str),
}

type Result<T> = std::result::Result<T, TypeCheckError>;

fn to_rational_exponent(exponent_f64: f64) -> Exponent {
    Rational::from_f64(exponent_f64).unwrap() // TODO
}

/// Evaluates a limited set of expressions *at compile time*. This is needed to support
/// type checking of expressions like `(2 * meter)^(2*3 - 4)` where we need to know not
/// just the *type* but also the *value* of the exponent.
fn evaluate_const_expr(expr: &typed_ast::Expression) -> Result<Exponent> {
    match expr {
        typed_ast::Expression::Scalar(n) => Ok(to_rational_exponent(n.to_f64())),
        typed_ast::Expression::Negate(ref expr, _) => Ok(-evaluate_const_expr(expr)?),
        typed_ast::Expression::BinaryOperator(op, lhs_expr, rhs_expr, _) => {
            let lhs = evaluate_const_expr(lhs_expr)?;
            let rhs = evaluate_const_expr(rhs_expr)?;
            match op {
                typed_ast::BinaryOperator::Add => Ok(lhs + rhs),
                typed_ast::BinaryOperator::Sub => Ok(lhs - rhs),
                typed_ast::BinaryOperator::Mul => Ok(lhs * rhs),
                typed_ast::BinaryOperator::Div => Ok(lhs / rhs), // TODO: type check error if rhs is zero
                typed_ast::BinaryOperator::Power => {
                    Err(TypeCheckError::UnsupportedConstExpression("exponentiation"))
                }
                typed_ast::BinaryOperator::ConvertTo => {
                    Err(TypeCheckError::UnsupportedConstExpression("conversion"))
                }
            }
        }
        typed_ast::Expression::Identifier(_, _) => {
            Err(TypeCheckError::UnsupportedConstExpression("identifier"))
        }
        typed_ast::Expression::FunctionCall(_, _, _) => {
            Err(TypeCheckError::UnsupportedConstExpression("function call"))
        }
    }
}

#[derive(Clone, Default)]
pub struct TypeChecker {
    types_for_identifier: HashMap<String, Type>,
    function_signatures: HashMap<String, (Vec<Type>, Type)>,
    registry: DimensionRegistry,
}

impl TypeChecker {
    fn type_for_identifier(&self, name: &str) -> Result<&Type> {
        self.types_for_identifier
            .get(name)
            .ok_or_else(|| TypeCheckError::UnknownIdentifier(name.into()))
    }

    pub(crate) fn check_expression(&self, ast: ast::Expression) -> Result<typed_ast::Expression> {
        Ok(match ast {
            ast::Expression::Scalar(n) => typed_ast::Expression::Scalar(n),
            ast::Expression::Identifier(name) => {
                let type_ = self.type_for_identifier(&name)?.clone();

                typed_ast::Expression::Identifier(name, type_)
            }
            ast::Expression::Negate(expr) => {
                let checked_expr = self.check_expression(*expr)?;
                let type_ = checked_expr.get_type();
                typed_ast::Expression::Negate(Box::new(checked_expr), type_)
            }
            ast::Expression::BinaryOperator(op, lhs, rhs) => {
                let lhs = self.check_expression(*lhs)?;
                let rhs = self.check_expression(*rhs)?;

                let get_type_and_assert_equality = || {
                    let lhs_type = lhs.get_type();
                    let rhs_type = rhs.get_type();
                    if lhs_type != rhs_type {
                        Err(TypeCheckError::IncompatibleDimensions(
                            "binary operator".into(),
                            " left hand side",
                            lhs_type,
                            "right hand side",
                            rhs_type,
                        ))
                    } else {
                        Ok(lhs_type)
                    }
                };

                let type_ = match op {
                    typed_ast::BinaryOperator::Add => get_type_and_assert_equality()?,
                    typed_ast::BinaryOperator::Sub => get_type_and_assert_equality()?,
                    typed_ast::BinaryOperator::Mul => lhs.get_type().multiply(rhs.get_type()),
                    typed_ast::BinaryOperator::Div => lhs.get_type().divide(rhs.get_type()),
                    typed_ast::BinaryOperator::Power => {
                        let exponent_type = rhs.get_type();
                        if exponent_type != Type::unity() {
                            return Err(TypeCheckError::NonScalarExponent(exponent_type.clone()));
                        }

                        let base_type = lhs.get_type();
                        if base_type == Type::unity() {
                            // Skip evaluating the exponent if the lhs is a scalar. This allows
                            // for arbitrary (decimal) exponents, if the base is a scalar.

                            base_type
                        } else {
                            let exponent = evaluate_const_expr(&rhs)?;
                            base_type.power(exponent)
                        }
                    }
                    typed_ast::BinaryOperator::ConvertTo => get_type_and_assert_equality()?,
                };

                typed_ast::Expression::BinaryOperator(op, Box::new(lhs), Box::new(rhs), type_)
            }
            ast::Expression::FunctionCall(function_name, args) => {
                let (parameter_types, return_type) =
                    self.function_signatures
                        .get(&function_name)
                        .ok_or_else(|| TypeCheckError::UnknownFunction(function_name.clone()))?;

                if parameter_types.len() != args.len() {
                    return Err(TypeCheckError::WrongArity(
                        function_name.clone(),
                        parameter_types.len(),
                        args.len(),
                    ));
                }

                let arguments_checked = args
                    .into_iter()
                    .map(|a| self.check_expression(a))
                    .collect::<Result<Vec<_>>>()?;
                let argument_types = arguments_checked.iter().map(|e| e.get_type());

                for (idx, (parameter_type, argument_type)) in
                    parameter_types.iter().zip(argument_types).enumerate()
                {
                    if *parameter_type != argument_type {
                        return Err(TypeCheckError::IncompatibleDimensions(
                            format!(
                                "argument {num} of function call to '{name}'",
                                num = idx + 1,
                                name = function_name
                            ),
                            "parameter type",
                            parameter_type.clone(),
                            " argument type",
                            argument_type,
                        ));
                    }
                }

                typed_ast::Expression::FunctionCall(
                    function_name,
                    arguments_checked,
                    return_type.clone(),
                )
            }
        })
    }

    pub fn check_statement(&mut self, ast: ast::Statement) -> Result<typed_ast::Statement> {
        Ok(match ast {
            ast::Statement::Expression(expr) => {
                typed_ast::Statement::Expression(self.check_expression(expr)?)
            }
            ast::Statement::DeclareVariable(name, expr, optional_dexpr) => {
                let expr = self.check_expression(expr)?;
                let type_deduced = expr.get_type();

                if let Some(ref dexpr) = optional_dexpr {
                    let type_specified = self
                        .registry
                        .get_base_representation(dexpr)
                        .map_err(TypeCheckError::RegistryError)?;
                    if type_deduced != type_specified {
                        return Err(TypeCheckError::IncompatibleDimensions(
                            "variable declaration".into(),
                            "specified dimension",
                            type_specified,
                            "   actual dimension",
                            type_deduced,
                        ));
                    }
                }
                self.types_for_identifier
                    .insert(name.clone(), type_deduced.clone());
                typed_ast::Statement::DeclareVariable(name, expr, type_deduced)
            }
            ast::Statement::DeclareDerivedUnit(name, expr, optional_dexpr) => {
                // TODO: this is the *exact same code* that we have above for
                // variable declarations => deduplicate this somehow
                let expr = self.check_expression(expr)?;
                let type_deduced = expr.get_type();

                if let Some(ref dexpr) = optional_dexpr {
                    let type_specified = self
                        .registry
                        .get_base_representation(dexpr)
                        .map_err(TypeCheckError::RegistryError)?;
                    if type_deduced != type_specified {
                        return Err(TypeCheckError::IncompatibleDimensions(
                            "derived unit declaration".into(),
                            "specified dimension",
                            type_specified,
                            "   actual dimension",
                            type_deduced,
                        ));
                    }
                }
                self.types_for_identifier
                    .insert(name.clone(), type_deduced.clone());
                typed_ast::Statement::DeclareDerivedUnit(name, expr, type_deduced)
            }
            ast::Statement::DeclareFunction(
                function_name,
                type_parameters,
                parameters,
                expr,
                optional_return_type_dexpr,
            ) => {
                let mut typechecker_fn = self.clone();
                let mut typed_parameters = vec![];
                for (parameter, optional_dexpr) in parameters {
                    let parameter_type = typechecker_fn
                        .registry
                        .get_base_representation(&optional_dexpr.unwrap())
                        .unwrap(); // TODO: error handling and parameter type deduction, in case it is not specified
                    typechecker_fn
                        .types_for_identifier
                        .insert(parameter.clone(), parameter_type.clone());
                    typed_parameters.push((parameter.clone(), parameter_type));
                }
                let expr = typechecker_fn.check_expression(expr)?;

                let return_type_deduced = expr.get_type();
                if let Some(ref return_type_dexpr) = optional_return_type_dexpr {
                    let return_type_specified = typechecker_fn
                        .registry
                        .get_base_representation(return_type_dexpr)
                        .map_err(TypeCheckError::RegistryError)?;

                    if return_type_deduced != return_type_specified {
                        return Err(TypeCheckError::IncompatibleDimensions(
                            "function return type".into(),
                            "specified return type",
                            return_type_specified,
                            "   actual return type",
                            return_type_deduced,
                        ));
                    }
                }

                let parameter_types = typed_parameters.iter().map(|(_, t)| t.clone()).collect();
                self.function_signatures.insert(
                    function_name.clone(),
                    (parameter_types, return_type_deduced.clone()),
                );

                typed_ast::Statement::DeclareFunction(
                    function_name,
                    typed_parameters,
                    expr,
                    return_type_deduced,
                )
            }
            ast::Statement::DeclareDimension(name, dexprs) => {
                if let Some(dexpr) = dexprs.first() {
                    self.registry
                        .add_derived_dimension(&name, dexpr)
                        .map_err(TypeCheckError::RegistryError)?;

                    let base_representation = self
                        .registry
                        .get_base_representation_for_name(&name)
                        .expect("we just inserted it");

                    for alternative_expr in &dexprs[1..] {
                        let alternative_base_representation = self
                            .registry
                            .get_base_representation(alternative_expr)
                            .map_err(TypeCheckError::RegistryError)?;
                        if alternative_base_representation != base_representation {
                            return Err(
                                TypeCheckError::IncompatibleAlternativeDimensionExpression(
                                    name.clone(),
                                ),
                            );
                        }
                    }
                } else {
                    self.registry
                        .add_base_dimension(&name)
                        .map_err(TypeCheckError::RegistryError)?;
                }
                typed_ast::Statement::DeclareDimension(name)
            }
            ast::Statement::DeclareBaseUnit(name, dexpr) => {
                let type_specified = self
                    .registry
                    .get_base_representation(&dexpr)
                    .map_err(TypeCheckError::RegistryError)?;
                self.types_for_identifier
                    .insert(name.clone(), type_specified.clone());
                typed_ast::Statement::DeclareBaseUnit(name, type_specified)
            }
        })
    }

    pub fn check_statements(
        &mut self,
        statements: impl IntoIterator<Item = ast::Statement>,
    ) -> Result<Vec<typed_ast::Statement>> {
        let mut statements_checked = vec![];

        for statement in statements.into_iter() {
            statements_checked.push(self.check_statement(statement)?);
        }
        Ok(statements_checked)
    }
}

pub fn typecheck(
    statements: impl IntoIterator<Item = ast::Statement>,
) -> Result<Vec<typed_ast::Statement>> {
    let mut typechecker = TypeChecker::default();
    typechecker.check_statements(statements)
}

#[cfg(test)]
const TEST_PRELUDE: &'static str = "
dimension A
dimension B
dimension C = A * B
unit a: A
unit b: B
unit c: C = a * b";

#[cfg(test)]
fn run_typecheck(input: &str) -> Result<typed_ast::Statement> {
    let code = &format!("{prelude}\n{input}", prelude = TEST_PRELUDE, input = input);
    let statements =
        crate::parser::parse(code).expect("No parse errors for inputs in this test suite");

    let mut typechecker = TypeChecker::default();
    typechecker
        .check_statements(statements)
        .map(|mut statements_checked| statements_checked.pop().unwrap())
}

#[cfg(test)]
fn assert_successful_typecheck(input: &str) {
    assert!(run_typecheck(input).is_ok());
}

#[cfg(test)]
fn get_typecheck_error(input: &str) -> TypeCheckError {
    if let Err(err) = run_typecheck(input) {
        err
    } else {
        panic!("Input was expected to yield a type check error");
    }
}

#[test]
fn basic() {
    use crate::registry::BaseRepresentationFactor;

    let type_a = BaseRepresentation::from_factor(BaseRepresentationFactor(
        "A".into(),
        Rational::from_integer(1),
    ));
    let type_b = BaseRepresentation::from_factor(BaseRepresentationFactor(
        "B".into(),
        Rational::from_integer(1),
    ));
    let type_c = type_a.clone().multiply(type_b.clone());

    assert_successful_typecheck(
        "let x: A = a\n\
                                 let y: B = b",
    );
    assert_successful_typecheck("let x: C = a * b");
    assert_successful_typecheck("let x: C = 2 * a * b^2 / b");
    assert_successful_typecheck("let x: A^3 = a^20 * a^(-17)");

    assert_successful_typecheck("a * b");
    assert_successful_typecheck("a / b");

    assert_successful_typecheck("fn f(x: A) -> A = x");
    assert_successful_typecheck("fn f(x: A) -> A·B = 2 * x * b");
    assert_successful_typecheck("fn f(x: A, y: B) -> C = x * y");

    assert!(matches!(
        get_typecheck_error("a + b"),
        TypeCheckError::IncompatibleDimensions(_, _, t1, _, t2) if t1 == type_a && t2 == type_b
    ));

    assert!(matches!(
        get_typecheck_error("fn f(x: A, y: B) -> C = x / y"),
        TypeCheckError::IncompatibleDimensions(_, _, t1, _, t2) if t1 == type_c && t2 == type_a.clone().divide(type_b.clone())
    ));

    assert!(matches!(
        get_typecheck_error("fn f(x: A) -> A = a\n\
                             f(b)"),
        TypeCheckError::IncompatibleDimensions(_, _, t1, _, t2) if t1 == type_a && t2 == type_b
    ));
}

#[test]
fn detects_wrong_alternative_expression() {
    assert!(matches!(
        get_typecheck_error(
            "# wrong alternative expression: C / B^2
             dimension D = A / B = C / B^3"
        ),
        TypeCheckError::IncompatibleAlternativeDimensionExpression(t) if t == "D",
    ));
}
