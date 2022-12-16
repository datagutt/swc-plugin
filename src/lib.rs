use std::any::Any;
use swc_core::common::Spanned;
use swc_core::ecma::{
    // ast::Program,
    ast::*,
    transforms::testing::test,
    visit::{as_folder, VisitMut, VisitMutWith},
};
use std::path::{Path, PathBuf};
use swc_core::{
    common::{FileName, DUMMY_SP, util::take::Take},
    ecma::{
        ast::*,
        atoms::JsWord,
        utils::{quote_ident, ExprFactory},
        visit::{Fold, FoldWith},
    },
    plugin::{
        metadata::TransformPluginMetadataContextKind, plugin_transform,
        proxies::TransformPluginProgramMetadata,
    },
};
use swc_core::ecma::utils::ExprExt;
// use swc_core::plugin::{plugin_transform, proxies::TransformPluginProgramMetadata};

const LINGUI_T: &str = &"t";

fn isLinguiFn(name: &str) -> bool {
    // todo: i didn't find a better way to create a constant hashmap
    match name {
        "plural" | "select" | "selectOrdinal" => true,
        _ => false,
    }
}

fn matchCalleeName(call: &CallExpr, fnName: &str) -> bool {
    match &call.callee {
        Callee::Expr(expr) => {
            if let Expr::Ident(ident) = expr.as_ref()  {
                return ident.sym.to_string() == fnName
            }
        },
        _ => {}
    }

    false
}

struct ValueWithPlaceholder {
    placeholder: String,
    value: Option<Box<Expr>>,
}

impl ValueWithPlaceholder {
    // Depending on whether value is presented it would produce or KeyValue or Shorthand exp
    fn to_prop(&self) -> PropOrSpread {
        let ident = Ident::new(self.placeholder.clone().into(), DUMMY_SP);

        PropOrSpread::Prop(Box::new(
            if let Some(e) = &self.value {
                Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(ident),
                    value: e.clone(),
                })
            } else {
                Prop::Shorthand(ident)
            }
        ))
    }
}

pub struct TransformVisitor;

impl TransformVisitor {
    // Receive an expression which expected to be either simple variable (ident) or expression
    // If simple variable is detected os literal used as placeholder
    // If expression detected we use index as placeholder.
    fn get_value_with_placeholder(&self, expr: Box<Expr>, i: &usize) -> ValueWithPlaceholder {
        match expr.as_ref() {
            // `text {foo} bar`
            Expr::Ident(ident) => {
                ValueWithPlaceholder {
                    placeholder: ident.sym.clone().to_string(),
                    value: None,
                }
            }
            // everything else, e.q.
            // `text {executeFn()} bar`
            // `text {bar.baz} bar`
            _ => {
                // would be a positional argument
                let index_str = &i.to_string()[..];

                ValueWithPlaceholder {
                    placeholder: index_str.into(),
                    value: Some(expr),
                }
            }
        }
    }

    // Receive TemplateLiteral with variables and return plane string where
    // substitutions replaced to placeholders and variables extracted as separate Vec
    // `Hello ${username}!` ->  (msg: `Hello {username}!`, variables: {username})
    fn transform_tpl_to_msg_and_values(&self, tpl: &Tpl) -> (String, Vec<PropOrSpread>) {
        let mut message = String::new();
        let mut values: Vec<&ValueWithPlaceholder> = Vec::with_capacity(tpl.exprs.len());
        let mut props = Vec::with_capacity(values.len());

        for (i, tplElement) in tpl.quasis.iter().enumerate() {
            message.push_str(&tplElement.raw);

            if let Some(exp) = tpl.exprs.get(i) {
                let val = self.get_value_with_placeholder(exp.clone(), &i);
                props.push(val.to_prop());
                message.push_str(&format!("{{{}}}", &val.placeholder));
            }
        }

        (message, props)
    }

    fn create_i18n_fn_call(&self, callee_obj: &Box<Expr>, message: &str, values: Vec<PropOrSpread>) -> CallExpr {
        return CallExpr {
            span: DUMMY_SP,
            callee: Expr::Member(MemberExpr {
                span: DUMMY_SP,
                obj: callee_obj.clone(),
                prop: MemberProp::Ident(Ident::new("_".into(), DUMMY_SP)),
            }).as_callee(),
            args: vec![
                message.as_arg(),
                Expr::Object(ObjectLit {
                    span: DUMMY_SP,
                    props: values,
                }).as_arg(),
            ],
            type_args: None,
        };
    }

    // receive ObjectLiteral {few: "..", many: "..", other: ".."} and create ICU string in form:
    // {count, plural, few {..} many {..} other {..}}
    // If messages passed as TemplateLiterals with variables, it extracts variables into Vec
    // (msg: {count, plural, one `{name} has # friend` other `{name} has # friends`}, variables: {name})
    fn get_icu_from_choices_obj(&self, props: &Vec<PropOrSpread>, icu_value_ident: &JsWord, icu_method: &JsWord) -> (String, Vec<PropOrSpread>) {
        let mut icuParts: Vec<String> = Vec::with_capacity(props.len());
        let mut all_values: Vec<PropOrSpread> = Vec::new();

        for propOrSpread in props {
            if let PropOrSpread::Prop(prop) = propOrSpread {
                if let Prop::KeyValue(prop) = prop.as_ref() {
                    if let PropName::Ident(ident) = &prop.key {
                        let mut push_part = |msg: &str| {
                            icuParts.push(format!("{} {{{}}}", ident.sym.to_string(), msg));
                        };

                        // String Literal: "has # friend"
                        if let Expr::Lit(lit) = prop.value.as_ref() {
                            if let Lit::Str(str) = lit {
                                // one {has # friend}
                                push_part(&str.value.to_string());
                            }
                        }

                        // Template Literal: `${name} has # friend`
                        if let Expr::Tpl(tpl) = prop.value.as_ref() {
                            let (msg, values) = self.transform_tpl_to_msg_and_values(tpl);
                            all_values.extend(values);
                            push_part(&msg);
                        }
                    } else {
                        // todo panic
                    }
                    // icuParts.push_str(prop.key)
                } else {
                    // todo: panic here we could not parse anything else then KeyValue pair
                }
            } else {
                // todo: panic here, we could not parse spread
            }
        }

        let msg = format!("{{{}, {}, {}}}", icu_value_ident, icu_method, icuParts.join(" "));

        println!("{}", msg);

        (msg, all_values)
    }
}

impl Fold for TransformVisitor {
    fn fold_expr(&mut self, mut expr: Expr) -> Expr {
        // If no package that we care about is imported, skip the following
        // transformation logic.
        // if self.import_packages.is_empty() {
        //     return expr;
        // }

        if let Expr::TaggedTpl(tagged_tpl) = &expr {
            match tagged_tpl.tag.as_ref() {
                // t(i18n)``
                Expr::Call(call) if matchCalleeName(call, LINGUI_T) => {
                    if let Some(v) = call.args.get(0) {
                        let (message, values)
                            = self.transform_tpl_to_msg_and_values(&tagged_tpl.tpl);
                        return Expr::Call(self.create_i18n_fn_call(
                            &v.expr,
                            &message,
                            values,
                        ));
                    }
                }
                // t``
                Expr::Ident(ident) if ident.sym.to_string() == LINGUI_T => {
                    let (message, values)
                        = self.transform_tpl_to_msg_and_values(&tagged_tpl.tpl);

                    return Expr::Call(self.create_i18n_fn_call(
                        &Box::new(Ident::new("i18n".into(), DUMMY_SP).into()),
                        &message,
                        values,
                    ));
                }
                _ => {}
            }
        }


        expr.fold_children_with(self)
    }

    fn fold_call_expr(&mut self, mut expr: CallExpr) -> CallExpr {
        // If no package that we care about is imported, skip the following
        // transformation logic.
        // if self.import_packages.is_empty() {
        //     return expr;
        // }

        if let Callee::Expr(e) = &expr.callee {
            match e.as_ref() {
                // (plural | select | selectOrdinal)()
                Expr::Ident(ident) => {
                    if !isLinguiFn(&ident.sym.to_string()) {
                        return expr;
                    }

                    if expr.args.len() != 2 {
                        // malformed plural call, exit
                        return expr;
                    }

                    // ICU value
                    let arg = expr.args.get(0).unwrap();
                    let icu_value
                        = self.get_value_with_placeholder(arg.expr.clone(), &0);

                    // ICU Choices
                    let arg = expr.args.get(1).unwrap();
                    if let Expr::Object(object) = &arg.expr.as_ref() {
                        let (message, values) = self.get_icu_from_choices_obj(
                            &object.props, &icu_value.placeholder.clone().into(), &ident.sym);

                        // todo need a function to remove duplicates from arguments
                        let mut allValues = vec![icu_value.to_prop()];
                        allValues.extend(values);

                        return self.create_i18n_fn_call(
                            &Box::new(Ident::new("i18n".into(), DUMMY_SP).into()),
                            &message,
                            allValues,
                        );
                    } else {
                        // todo passed not an ObjectLiteral,
                        //      we should panic here or just skip this call
                    }
                }
                _ => {}
            }
        }

        expr
    }
}


/// An example plugin function with macro support.
/// `plugin_transform` macro interop pointers into deserialized structs, as well
/// as returning ptr back to host.
///
/// It is possible to opt out from macro by writing transform fn manually
/// if plugin need to handle low-level ptr directly via
/// `__transform_plugin_process_impl(
///     ast_ptr: *const u8, ast_ptr_len: i32,
///     unresolved_mark: u32, should_enable_comments_proxy: i32) ->
///     i32 /*  0 for success, fail otherwise.
///             Note this is only for internal pointer interop result,
///             not actual transform result */`
///
/// This requires manual handling of serialization / deserialization from ptrs.
/// Refer swc_plugin_macro to see how does it work internally.
#[plugin_transform]
pub fn process_transform(program: Program, _metadata: TransformPluginProgramMetadata) -> Program {
    program.fold_with(&mut TransformVisitor)
}

test!(
    Default::default(),
    |_| TransformVisitor,
    should_not_touch_not_related_tagget_tpls,
    // input
     r#"
     b`Refresh inbox`;
     b(i18n)`Refresh inbox`;
     "#,
    // output after transform
    r#"
    b`Refresh inbox`;
    b(i18n)`Refresh inbox`;
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor,
    substitution_in_tpl_literal1,
    // input
     r#"
     t`Refresh inbox`
     t`Refresh ${foo} inbox ${bar}`
     t`Refresh ${foo.bar} inbox ${bar}`
     t`Refresh ${expr()}`
     "#,
    // output after transform
    r#"
    i18n._("Refresh inbox", {})
    i18n._("Refresh {foo} inbox {bar}", {foo, bar})
    i18n._("Refresh {0} inbox {bar}", {0: foo.bar, bar})
    i18n._("Refresh {0}", {0: expr()})
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor,
    custom_i18n_passed,
    // input
     r#"
     t(custom_i18n)`Refresh inbox`
     t(custom_i18n)`Refresh ${foo} inbox ${bar}`
     t(custom_i18n)`Refresh ${foo.bar} inbox ${bar}`
     t(custom_i18n)`Refresh ${expr()}`
     "#,
    // output after transform
    r#"
    custom_i18n._("Refresh inbox", {})
    custom_i18n._("Refresh {foo} inbox {bar}", {foo, bar})
    custom_i18n._("Refresh {0} inbox {bar}", {0: foo.bar, bar})
    custom_i18n._("Refresh {0}", {0: expr()})
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor,
    icu_functions,
     r#"
    const messagePlural = plural(count, {
       one: '# Book',
       other: '# Books'
    })
    const messageSelect = select(gender, {
       male: 'he',
       female: 'she',
       other: 'they'
    })
    const messageSelectOrdinal = selectOrdinal(count, {
       one: '#st',
       two: '#nd',
       few: '#rd',
       other: '#th',
    })
     "#,
    r#"
    const messagePlural = i18n._("{count, plural, one {# Book} other {# Books}}", {
      count
    });
    const messageSelect = i18n._("{gender, select, male {he} female {she} other {they}}", {
      gender
    });
    const messageSelectOrdinal = i18n._("{count, selectOrdinal, one {#st} two {#nd} few {#rd} other {#th}}", {
      count
    });
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor,
    should_not_touch_non_lungui_fns,
     r#"
    const messagePlural = customName(count, {
       one: '# Book',
       other: '# Books'
    })
     "#,
    r#"
   const messagePlural = customName(count, {
       one: '# Book',
       other: '# Books'
    })
    "#
);

test!(
    Default::default(),
    |_| TransformVisitor,
    plural_with_placeholders,
     r#"
       const message = plural(count, {
           one: `${name} has # friend`,
           other: `${name} has # friends`
        })
     "#,
    r#"
    const message = i18n._("{count, plural, one {{name} has # friend} other {{name} has # friends}}", {
      count, name
    })
    "#
);