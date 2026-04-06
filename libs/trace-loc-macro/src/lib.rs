use proc_macro::TokenStream;
use quote::{ToTokens, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Stmt, parse_macro_input};

#[proc_macro]
pub fn trace_loc(input: TokenStream) -> TokenStream {
    let input2 = proc_macro2::TokenStream::from(input.clone());
    if input2.is_empty() {
        return quote!({
            crate::debug::record_trace_location(file!(), line!(), column!());
        })
        .into();
    }

    let parser = |input: syn::parse::ParseStream<'_>| -> syn::Result<Vec<Stmt>> {
        syn::Block::parse_within(input)
    };
    let statements = parse_macro_input!(input with parser);

    if statements.is_empty() {
        return quote!({
            crate::debug::record_trace_location(file!(), line!(), column!());
        })
        .into();
    }

    let mut expanded = Vec::with_capacity(statements.len() * 2);
    for stmt in statements {
        let span = stmt.span();
        let note = syn::LitStr::new(&stmt.to_token_stream().to_string(), span);
        expanded.push(quote_spanned! {span=>
            crate::debug::record_trace_location_with_note(
                file!(),
                line!(),
                column!(),
                Some(#note)
            );
        });
        expanded.push(quote! { #stmt });
    }

    quote!({
        #(#expanded)*
    })
    .into()
}
