use aster;

use syntax::ast::{
    self,
    EnumDef,
    Ident,
    Item,
    MetaItem,
};
use syntax::codemap::Span;
use syntax::ext::base::{Annotatable, ExtCtxt};
use syntax::ext::build::AstBuilder;
use syntax::parse::token::InternedString;
use syntax::ptr::P;

use attr;
use error::Error;

pub fn expand_derive_deserialize(
    cx: &mut ExtCtxt,
    span: Span,
    meta_item: &MetaItem,
    annotatable: &Annotatable,
    push: &mut FnMut(Annotatable)
) {
    let item = match *annotatable {
        Annotatable::Item(ref item) => item,
        _ => {
            cx.span_err(
                meta_item.span,
                "`#[derive(Deserialize)]` may only be applied to structs and enums");
            return;
        }
    };

    let builder = aster::AstBuilder::new().span(span);

    let generics = match item.node {
        ast::ItemKind::Struct(_, ref generics) => generics,
        ast::ItemKind::Enum(_, ref generics) => generics,
        _ => {
            cx.span_err(
                meta_item.span,
                "`#[derive(Deserialize)]` may only be applied to structs and enums");
            return;
        }
    };

    let impl_generics = builder.from_generics(generics.clone())
        .add_ty_param_bound(
            builder.path().global().ids(&["serde", "de", "Deserialize"]).build()
        )
        .build();

    let ty = builder.ty().path()
        .segment(item.ident).with_generics(impl_generics.clone()).build()
        .build();

    let body = match deserialize_body(cx, &builder, &item, &impl_generics, ty.clone()) {
        Ok(body) => body,
        Err(Error) => {
            // An error occured, but it should have been reported already.
            return;
        }
    };

    let where_clause = &impl_generics.where_clause;

    let impl_item = quote_item!(cx,
        impl $impl_generics ::serde::de::Deserialize for $ty $where_clause {
            fn deserialize<__D>(deserializer: &mut __D) -> ::std::result::Result<$ty, __D::Error>
                where __D: ::serde::de::Deserializer,
            {
                $body
            }
        }
    ).unwrap();

    push(Annotatable::Item(impl_item))
}

fn deserialize_body(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    item: &Item,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
) -> Result<P<ast::Expr>, Error> {
    let container_attrs = try!(attr::ContainerAttrs::from_item(cx, item));

    match item.node {
        ast::ItemKind::Struct(ref variant_data, _) => {
            deserialize_item_struct(
                cx,
                builder,
                item,
                impl_generics,
                ty,
                item.span,
                variant_data,
                &container_attrs,
            )
        }
        ast::ItemKind::Enum(ref enum_def, _) => {
            deserialize_item_enum(
                cx,
                builder,
                item.ident,
                impl_generics,
                ty,
                enum_def,
                &container_attrs,
            )
        }
        _ => {
            cx.span_bug(item.span,
                        "expected ItemStruct or ItemEnum in #[derive(Deserialize)]")
        }
    }
}

fn deserialize_item_struct(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    item: &Item,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
    span: Span,
    variant_data: &ast::VariantData,
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    match *variant_data {
        ast::VariantData::Unit(_) => {
            deserialize_unit_struct(
                cx,
                item.ident,
                container_attrs,
            )
        }
        ast::VariantData::Tuple(ref fields, _) if fields.len() == 1 => {
            deserialize_newtype_struct(
                cx,
                &builder,
                item.ident,
                impl_generics,
                ty,
                container_attrs,
            )
        }
        ast::VariantData::Tuple(ref fields, _) => {
            if fields.iter().any(|field| !field.node.kind.is_unnamed()) {
                cx.span_bug(span, "tuple struct has named fields")
            }

            deserialize_tuple_struct(
                cx,
                &builder,
                item.ident,
                impl_generics,
                ty,
                fields.len(),
                container_attrs,
            )
        }
        ast::VariantData::Struct(ref fields, _) => {
            if fields.iter().any(|field| field.node.kind.is_unnamed()) {
                cx.span_bug(span, "struct has unnamed fields")
            }

            deserialize_struct(
                cx,
                &builder,
                item.ident,
                impl_generics,
                ty,
                fields,
                container_attrs,
            )
        }
    }
}


// Build `__Visitor<A, B, ...>(PhantomData<A>, PhantomData<B>, ...)`
fn deserialize_visitor(
    builder: &aster::AstBuilder,
    trait_generics: &ast::Generics,
    forward_ty_params: Vec<ast::TyParam>,
    forward_tys: Vec<P<ast::Ty>>
) -> Result<(P<ast::Item>, P<ast::Ty>, P<ast::Expr>, ast::Generics), Error> {
    if trait_generics.ty_params.is_empty() && forward_tys.is_empty() {
        Ok((
            builder.item().tuple_struct("__Visitor").build(),
            builder.ty().id("__Visitor"),
            builder.expr().id("__Visitor"),
            trait_generics.clone(),
        ))
    } else {
        let placeholders : Vec<_> = trait_generics.ty_params.iter()
            .map(|t| builder.ty().id(t.ident))
            .collect();
        let mut trait_generics = trait_generics.clone();
        let mut ty_params = forward_ty_params.clone();
        ty_params.extend(trait_generics.ty_params.into_vec());
        trait_generics.ty_params = P::from_vec(ty_params);

        Ok((
            builder.item().tuple_struct("__Visitor")
                .generics().with(trait_generics.clone()).build()
                .with_tys({
                    let lifetimes = trait_generics.lifetimes.iter()
                        .map(|lifetime_def| {
                            builder.ty()
                                .phantom_data()
                                .ref_().lifetime(lifetime_def.lifetime.name)
                                .ty()
                                .unit()
                        });

                    let ty_params = trait_generics.ty_params.iter()
                        .map(|ty_param| {
                            builder.ty()
                                .phantom_data()
                                .id(ty_param.ident)
                        });

                    lifetimes.chain(ty_params)
                })
                .build(),
            builder.ty().path()
                .segment("__Visitor").with_generics(trait_generics.clone()).build()
                .build(),
            builder.expr().call()
                .path().segment("__Visitor")
                .with_tys(forward_tys)
                .with_tys(placeholders)
                .build().build()
                .with_args({
                    let len = trait_generics.lifetimes.len() + trait_generics.ty_params.len();

                    (0 .. len).map(|_| builder.expr().phantom_data())
                })
                .build(),
            trait_generics,
        ))
    }
}

fn deserializer_ty_param(builder: &aster::AstBuilder) -> ast::TyParam {
    builder.ty_param("__D")
        .trait_bound(builder.path()
                     .global()
                     .segment("serde").build()
                     .segment("de").build()
                     .id("Deserializer")
                     .build())
        .build()
        .build()
}

fn deserializer_ty_arg(builder: &aster::AstBuilder) -> P<ast::Ty>{
    builder.ty().id("__D")
}

fn deserialize_unit_struct(
    cx: &ExtCtxt,
    type_ident: Ident,
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let type_name = container_attrs.name().deserialize_name_expr();

    Ok(quote_expr!(cx, {
        struct __Visitor;

        impl ::serde::de::Visitor for __Visitor {
            type Value = $type_ident;

            #[inline]
            fn visit_unit<E>(&mut self) -> ::std::result::Result<$type_ident, E>
                where E: ::serde::de::Error,
            {
                Ok($type_ident)
            }

            #[inline]
            fn visit_seq<V>(&mut self, mut visitor: V) -> ::std::result::Result<$type_ident, V::Error>
                where V: ::serde::de::SeqVisitor,
            {
                try!(visitor.end());
                self.visit_unit()
            }
        }

        deserializer.deserialize_unit_struct($type_name, __Visitor)
    }))
}

fn deserialize_newtype_struct(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: Ident,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &impl_generics.where_clause;

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        impl_generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    let visit_seq_expr = deserialize_seq(
        cx,
        builder,
        builder.path().id(type_ident).build(),
        1,
    );

    let type_name = container_attrs.name().deserialize_name_expr();

    Ok(quote_expr!(cx, {
        $visitor_item

        impl $visitor_generics ::serde::de::Visitor for $visitor_ty $where_clause {
            type Value = $ty;

            #[inline]
            fn visit_newtype_struct<D>(&mut self, deserializer: &mut D) -> ::std::result::Result<Self::Value, D::Error>
                where D: ::serde::de::Deserializer,
            {
                let value = try!(::serde::de::Deserialize::deserialize(deserializer));
                Ok($type_ident(value))
            }

            #[inline]
            fn visit_seq<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::SeqVisitor,
            {
                $visit_seq_expr
            }
        }

        deserializer.deserialize_newtype_struct($type_name, $visitor_expr)
    }))
}

fn deserialize_tuple_struct(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: Ident,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
    fields: usize,
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &impl_generics.where_clause;

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        impl_generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    let visit_seq_expr = deserialize_seq(
        cx,
        builder,
        builder.path().id(type_ident).build(),
        fields,
    );

    let type_name = container_attrs.name().deserialize_name_expr();

    Ok(quote_expr!(cx, {
        $visitor_item

        impl $visitor_generics ::serde::de::Visitor for $visitor_ty $where_clause {
            type Value = $ty;

            #[inline]
            fn visit_seq<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::SeqVisitor,
            {
                $visit_seq_expr
            }
        }

        deserializer.deserialize_tuple_struct($type_name, $fields, $visitor_expr)
    }))
}

fn deserialize_seq(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    struct_path: ast::Path,
    fields: usize,
) -> P<ast::Expr> {
    let let_values: Vec<ast::Stmt> = (0 .. fields)
        .map(|i| {
            let name = builder.id(format!("__field{}", i));
            quote_stmt!(cx,
                let $name = match try!(visitor.visit()) {
                    Some(value) => { value },
                    None => {
                        return Err(::serde::de::Error::end_of_stream());
                    }
                };
            ).unwrap()
        })
        .collect();

    let result = builder.expr().call()
        .build_path(struct_path)
        .with_args((0 .. fields).map(|i| builder.expr().id(format!("__field{}", i))))
        .build();

    quote_expr!(cx, {
        $let_values

        try!(visitor.end());

        Ok($result)
    })
}

fn deserialize_struct_as_seq(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    struct_path: ast::Path,
    fields: &[ast::StructField],
) -> Result<P<ast::Expr>, Error> {
    let let_values: Vec<_> = (0 .. fields.len())
        .map(|i| {
            let name = builder.id(format!("__field{}", i));
            quote_stmt!(cx,
                let $name = match try!(visitor.visit()) {
                    Some(value) => { value },
                    None => {
                        return Err(::serde::de::Error::end_of_stream());
                    }
                };
            ).unwrap()
        })
        .collect();

    let result = builder.expr().struct_path(struct_path)
        .with_id_exprs(
            fields.iter()
                .enumerate()
                .map(|(i, field)| {
                    (
                        match field.node.kind {
                            ast::NamedField(name, _) => name.clone(),
                            ast::UnnamedField(_) => {
                                cx.span_bug(field.span, "struct contains unnamed fields")
                            }
                        },
                        builder.expr().id(format!("__field{}", i)),
                    )
                })
        )
        .build();

    Ok(quote_expr!(cx, {
        $let_values

        try!(visitor.end());

        Ok($result)
    }))
}

fn deserialize_struct(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: Ident,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
    fields: &[ast::StructField],
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &impl_generics.where_clause;

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        &impl_generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    let type_path = builder.path().id(type_ident).build();

    let visit_seq_expr = try!(deserialize_struct_as_seq(
        cx,
        builder,
        type_path.clone(),
        fields,
    ));

    let (field_visitor, fields_stmt, visit_map_expr) = try!(deserialize_struct_visitor(
        cx,
        builder,
        type_path.clone(),
        &ty,
        impl_generics,
        fields,
        container_attrs,
        false,
    ));

    let type_name = container_attrs.name().deserialize_name_expr();

    Ok(quote_expr!(cx, {
        $field_visitor

        $visitor_item

        impl $visitor_generics ::serde::de::Visitor for $visitor_ty $where_clause {
            type Value = $ty;

            #[inline]
            fn visit_seq<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::SeqVisitor,
            {
                $visit_seq_expr
            }

            #[inline]
            fn visit_map<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::MapVisitor,
            {
                $visit_map_expr
            }
        }

        $fields_stmt

        deserializer.deserialize_struct($type_name, FIELDS, $visitor_expr)
    }))
}

fn deserialize_item_enum(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: Ident,
    impl_generics: &ast::Generics,
    ty: P<ast::Ty>,
    enum_def: &EnumDef,
    container_attrs: &attr::ContainerAttrs
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &impl_generics.where_clause;

    let type_name = container_attrs.name().deserialize_name_expr();

    let variant_visitor = deserialize_field_visitor(
        cx,
        builder,
        try!(
            enum_def.variants.iter()
                .map(|variant| {
                    let attrs = try!(attr::VariantAttrs::from_variant(cx, variant));
                    Ok(attrs.name().deserialize_name())
                })
                .collect()
        ),
        container_attrs,
        true,
    );

    let variants_expr = builder.expr().ref_().slice()
        .with_exprs(
            enum_def.variants.iter()
                .map(|variant| {
                    builder.expr().str(variant.node.name)
                })
        )
        .build();

    let variants_stmt = quote_stmt!(cx,
        const VARIANTS: &'static [&'static str] = $variants_expr;
    ).unwrap();

    let ignored_arm = if !container_attrs.deny_unknown_fields() {
        Some(quote_arm!(cx, __Field::__ignore => { Err(::serde::de::Error::end_of_stream()) }))
    } else {
        None
    };

    // Match arms to extract a variant from a string
    let mut variant_arms = vec![];
    for (i, variant) in enum_def.variants.iter().enumerate() {
        let variant_name = builder.pat().path()
            .id("__Field").id(format!("__field{}", i))
            .build();

        let expr = try!(deserialize_variant(
            cx,
            builder,
            type_ident,
            impl_generics,
            ty.clone(),
            variant,
            container_attrs,
        ));

        let arm = quote_arm!(cx, $variant_name => { $expr });
        variant_arms.push(arm);
    }
    variant_arms.extend(ignored_arm.into_iter());

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        impl_generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    Ok(quote_expr!(cx, {
        $variant_visitor

        $visitor_item

        impl $visitor_generics ::serde::de::EnumVisitor for $visitor_ty $where_clause {
            type Value = $ty;

            fn visit<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::VariantVisitor,
            {
                match try!(visitor.visit_variant()) {
                    $variant_arms
                }
            }
        }

        $variants_stmt

        deserializer.deserialize_enum($type_name, VARIANTS, $visitor_expr)
    }))
}

fn deserialize_variant(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: Ident,
    generics: &ast::Generics,
    ty: P<ast::Ty>,
    variant: &ast::Variant,
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let variant_ident = variant.node.name;

    match variant.node.data {
        ast::VariantData::Unit(_) => {
            Ok(quote_expr!(cx, {
                try!(visitor.visit_unit());
                Ok($type_ident::$variant_ident)
            }))
        }
        ast::VariantData::Tuple(ref args, _) if args.len() == 1 => {
            Ok(quote_expr!(cx, {
                let val = try!(visitor.visit_newtype());
                Ok($type_ident::$variant_ident(val))
            }))
        }
        ast::VariantData::Tuple(ref fields, _) => {
            deserialize_tuple_variant(
                cx,
                builder,
                type_ident,
                variant_ident,
                generics,
                ty,
                fields.len(),
            )
        }
        ast::VariantData::Struct(ref fields, _) => {
            deserialize_struct_variant(
                cx,
                builder,
                type_ident,
                variant_ident,
                generics,
                ty,
                fields,
                container_attrs,
            )
        }
    }
}

fn deserialize_tuple_variant(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: ast::Ident,
    variant_ident: ast::Ident,
    generics: &ast::Generics,
    ty: P<ast::Ty>,
    fields: usize,
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &generics.where_clause;

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    let visit_seq_expr = deserialize_seq(
        cx,
        builder,
        builder.path().id(type_ident).id(variant_ident).build(),
        fields,
    );

    Ok(quote_expr!(cx, {
        $visitor_item

        impl $visitor_generics ::serde::de::Visitor for $visitor_ty $where_clause {
            type Value = $ty;

            fn visit_seq<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::SeqVisitor,
            {
                $visit_seq_expr
            }
        }

        visitor.visit_tuple($fields, $visitor_expr)
    }))
}

fn deserialize_struct_variant(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    type_ident: ast::Ident,
    variant_ident: ast::Ident,
    generics: &ast::Generics,
    ty: P<ast::Ty>,
    fields: &[ast::StructField],
    container_attrs: &attr::ContainerAttrs,
) -> Result<P<ast::Expr>, Error> {
    let where_clause = &generics.where_clause;

    let type_path = builder.path()
        .id(type_ident)
        .id(variant_ident)
        .build();

    let visit_seq_expr = try!(deserialize_struct_as_seq(
        cx,
        builder,
        type_path.clone(),
        fields,
    ));

    let (field_visitor, fields_stmt, field_expr) = try!(deserialize_struct_visitor(
        cx,
        builder,
        type_path,
        &ty,
        generics,
        fields,
        container_attrs,
        true,
    ));

    let (visitor_item, visitor_ty, visitor_expr, visitor_generics) = try!(deserialize_visitor(
        builder,
        generics,
        vec![deserializer_ty_param(builder)],
        vec![deserializer_ty_arg(builder)],
    ));

    Ok(quote_expr!(cx, {
        $field_visitor

        $visitor_item

        impl $visitor_generics ::serde::de::Visitor for $visitor_ty $where_clause {
            type Value = $ty;

            #[inline]
            fn visit_seq<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::SeqVisitor,
            {
                $visit_seq_expr
            }

            #[inline]
            fn visit_map<__V>(&mut self, mut visitor: __V) -> ::std::result::Result<$ty, __V::Error>
                where __V: ::serde::de::MapVisitor,
            {
                $field_expr
            }
        }

        $fields_stmt

        visitor.visit_struct(FIELDS, $visitor_expr)
    }))
}

fn deserialize_field_visitor(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    field_names: Vec<InternedString>,
    container_attrs: &attr::ContainerAttrs,
    is_variant: bool,
) -> Vec<P<ast::Item>> {
    // Create the field names for the fields.
    let field_idents: Vec<_> = (0 .. field_names.len())
        .map(|i| builder.id(format!("__field{}", i)))
        .collect();

    let ignore_variant = if !container_attrs.deny_unknown_fields() {
        let skip_ident = builder.id("__ignore");
        Some(builder.variant(skip_ident).unit())
    } else {
        None
    };

    let field_enum = builder.item()
        .attr().allow(&["non_camel_case_types"])
        .enum_("__Field")
        .with_variants(
            field_idents.iter().map(|field_ident| {
                builder.variant(field_ident).unit()
            })
        )
        .with_variants(ignore_variant.into_iter())
        .build();

    let index_field_arms: Vec<_> = field_idents.iter()
        .enumerate()
        .map(|(field_index, field_ident)| {
            quote_arm!(cx, $field_index => { Ok(__Field::$field_ident) })
        })
        .collect();

    let (index_error_msg, unknown_ident) = if is_variant {
        (builder.expr().str("expected a variant"), builder.id("unknown_variant"))
    } else {
        (builder.expr().str("expected a field"), builder.id("unknown_field"))
    };

    let fallthrough_index_arm_expr = if !is_variant && !container_attrs.deny_unknown_fields() {
        quote_expr!(cx, Ok(__Field::__ignore))
    } else {
        quote_expr!(cx, {
            Err(::serde::de::Error::invalid_value($index_error_msg))
        })
    };

    let index_body = quote_expr!(cx,
        match value {
            $index_field_arms
            _ => $fallthrough_index_arm_expr
        }
    );

    // Convert the field names into byte strings.
    let str_field_names: Vec<_> = field_names.iter()
        .map(|name| builder.expr().lit().str(&name))
        .collect();

    // Match arms to extract a field from a string
    let str_field_arms: Vec<_> = field_idents.iter().zip(str_field_names.iter())
        .map(|(field_ident, field_name)| {
            quote_arm!(cx, $field_name => { Ok(__Field::$field_ident) })
        })
        .collect();

    let fallthrough_str_arm_expr = if !is_variant && !container_attrs.deny_unknown_fields() {
        quote_expr!(cx, Ok(__Field::__ignore))
    } else {
        quote_expr!(cx, Err(::serde::de::Error::$unknown_ident(value)))
    };

    let str_body = quote_expr!(cx,
        match value {
            $str_field_arms
            _ => $fallthrough_str_arm_expr
        }
    );

    // Convert the field names into byte strings.
    let bytes_field_names: Vec<_> = field_names.iter()
        .map(|name| {
            let name: &str = name;
            builder.expr().lit().byte_str(name)
        })
        .collect();

    // Match arms to extract a field from a string
    let bytes_field_arms: Vec<_> = field_idents.iter().zip(bytes_field_names.iter())
        .map(|(field_ident, field_name)| {
            quote_arm!(cx, $field_name => { Ok(__Field::$field_ident) })
        })
        .collect();

    let fallthrough_bytes_arm_expr = if !is_variant && !container_attrs.deny_unknown_fields() {
        quote_expr!(cx, Ok(__Field::__ignore))
    } else {
        quote_expr!(cx, {
            let value = ::std::string::String::from_utf8_lossy(value);
            Err(::serde::de::Error::$unknown_ident(&value))
        })
    };

    let bytes_body = quote_expr!(cx,
        match value {
            $bytes_field_arms
            _ => $fallthrough_bytes_arm_expr
        }
    );

    let impl_item = quote_item!(cx,
        impl ::serde::de::Deserialize for __Field {
            #[inline]
            fn deserialize<D>(deserializer: &mut D) -> ::std::result::Result<__Field, D::Error>
                where D: ::serde::de::Deserializer,
            {
                use std::marker::PhantomData;

                struct __FieldVisitor<D> {
                    phantom: PhantomData<D>
                }

                impl<__D> ::serde::de::Visitor for __FieldVisitor<__D>
                    where __D: ::serde::de::Deserializer
                {
                    type Value = __Field;

                    fn visit_usize<E>(&mut self, value: usize) -> ::std::result::Result<__Field, E>
                        where E: ::serde::de::Error,
                    {
                        $index_body
                    }

                    fn visit_str<E>(&mut self, value: &str) -> ::std::result::Result<__Field, E>
                        where E: ::serde::de::Error,
                    {
                        $str_body
                    }

                    fn visit_bytes<E>(&mut self, value: &[u8]) -> ::std::result::Result<__Field, E>
                        where E: ::serde::de::Error,
                    {
                        $bytes_body
                    }
                }

                deserializer.deserialize_struct_field(__FieldVisitor::<D>{ phantom: PhantomData })
            }
        }
    ).unwrap();

    vec![field_enum, impl_item]
}

fn deserialize_struct_visitor(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    struct_path: ast::Path,
    container_ty: &P<ast::Ty>,
    generics: &ast::Generics,
    fields: &[ast::StructField],
    container_attrs: &attr::ContainerAttrs,
    is_enum: bool,
) -> Result<(Vec<P<ast::Item>>, ast::Stmt, P<ast::Expr>), Error> {
    let field_exprs = fields.iter()
        .map(|field| {
            let field_attrs = try!(
                attr::FieldAttrs::from_field(cx,
                                             container_ty,
                                             generics,
                                             field,
                                             is_enum)
            );
            Ok(field_attrs.name().deserialize_name())
        })
        .collect();

    let field_visitor = deserialize_field_visitor(
        cx,
        builder,
        try!(field_exprs),
        container_attrs,
        false,
    );

    let visit_map_expr = try!(deserialize_map(
        cx,
        builder,
        struct_path,
        container_ty,
        generics,
        fields,
        container_attrs,
        is_enum,
    ));

    let fields_expr = builder.expr().ref_().slice()
        .with_exprs(
            fields.iter()
                .map(|field| {
                    match field.node.kind {
                        ast::NamedField(name, _) => builder.expr().str(name),
                        ast::UnnamedField(_) => {
                            cx.span_bug(field.span, "struct contains unnamed fields")
                        }
                    }
                })
        )
        .build();

    let fields_stmt = quote_stmt!(cx,
        const FIELDS: &'static [&'static str] = $fields_expr;
    ).unwrap();

    Ok((field_visitor, fields_stmt, visit_map_expr))
}

fn deserialize_map(
    cx: &ExtCtxt,
    builder: &aster::AstBuilder,
    struct_path: ast::Path,
    container_ty: &P<ast::Ty>,
    generics: &ast::Generics,
    fields: &[ast::StructField],
    container_attrs: &attr::ContainerAttrs,
    is_enum: bool,
) -> Result<P<ast::Expr>, Error> {
    // Create the field names for the fields.
    let field_names: Vec<ast::Ident> = (0 .. fields.len())
        .map(|i| builder.id(format!("__field{}", i)))
        .collect();

    let field_attrs: Vec<_> = try!(
        fields.iter()
            .map(|field| attr::FieldAttrs::from_field(cx, container_ty, generics, field, is_enum))
            .collect()
    );

    // Declare each field.
    let let_values: Vec<ast::Stmt> = field_names.iter()
        .map(|field_name| quote_stmt!(cx, let mut $field_name = None;).unwrap())
        .collect();


    // Visit ignored values to consume them
    let ignored_arm = if !container_attrs.deny_unknown_fields() {
        Some(quote_arm!(cx,
            _ => { try!(visitor.visit_value::<::serde::de::impls::IgnoredAny>()); }
        ))
    } else {
        None
    };

    // Match arms to extract a value for a field.
    let value_arms = field_attrs.iter().zip(field_names.iter())
        .map(|(field_attr, field_name)| {
            let expr = match field_attr.deserialize_with() {
                Some(expr) => expr.clone(),
                None => quote_expr!(cx, visitor.visit_value()),
            };

            quote_arm!(cx,
                __Field::$field_name => {
                    $field_name = Some(try!($expr));
                }
            )
        })
        .chain(ignored_arm.into_iter())
        .collect::<Vec<_>>();

    let extract_values = field_attrs.iter().zip(field_names.iter())
        .map(|(field_attr, field_name)| {
            let missing_expr = field_attr.expr_is_missing();

            Ok(quote_stmt!(cx,
                let $field_name = match $field_name {
                    Some($field_name) => $field_name,
                    None => $missing_expr
                };
            ).unwrap())
        })
        .collect::<Result<Vec<_>, _>>();

    let extract_values = try!(extract_values);

    let result = builder.expr().struct_path(struct_path)
        .with_id_exprs(
            fields.iter()
                .zip(field_names.iter())
                .map(|(field, field_name)| {
                    (
                        match field.node.kind {
                            ast::NamedField(name, _) => name.clone(),
                            ast::UnnamedField(_) => {
                                cx.span_bug(field.span, "struct contains unnamed fields")
                            }
                        },
                        builder.expr().id(field_name),
                    )
                })
        )
        .build();

    Ok(quote_expr!(cx, {
        $let_values

        while let Some(key) = try!(visitor.visit_key()) {
            match key {
                $value_arms
            }
        }

        $extract_values

        try!(visitor.end());

        Ok($result)
    }))
}
