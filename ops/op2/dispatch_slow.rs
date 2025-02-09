// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
use super::generator_state::GeneratorState;
use super::signature::Arg;
use super::signature::NumericArg;
use super::signature::ParsedSignature;
use super::signature::RefType;
use super::signature::RetVal;
use super::signature::Special;
use super::signature::V8Arg;
use super::MacroConfig;
use super::V8MappingError;
use proc_macro2::Ident;
use proc_macro2::TokenStream;
use quote::format_ident;
use quote::quote;

pub(crate) fn generate_dispatch_slow(
  config: &MacroConfig,
  generator_state: &mut GeneratorState,
  signature: &ParsedSignature,
) -> Result<TokenStream, V8MappingError> {
  let mut output = TokenStream::new();

  // Fast ops require the slow op to check op_ctx for the last error
  if config.fast && matches!(signature.ret_val, RetVal::Result(_)) {
    generator_state.needs_opctx = true;
    let throw_exception = throw_exception(generator_state)?;
    // If the fast op returned an error, we must throw it rather than doing work.
    output.extend(quote!{
      // FASTCALL FALLBACK: This is where we pick up the errors for the slow-call error pickup
      // path. There is no code running between this and the other FASTCALL FALLBACK comment,
      // except some V8 code required to perform the fallback process. This is why the below call is safe.

      // SAFETY: We guarantee that OpCtx has no mutable references once ops are live and being called,
      // allowing us to perform this one little bit of mutable magic.
      if let Some(err) = unsafe { opctx.unsafely_take_last_error_for_ops_only() } {
        #throw_exception
      }
    });
  }

  // Collect virtual arguments in a deferred list that we compute at the very end. This allows us to copy
  // the scope borrow.
  let mut deferred = TokenStream::new();
  let mut input_index = 0;

  for (index, arg) in signature.args.iter().enumerate() {
    if arg.is_virtual() {
      deferred.extend(from_arg(generator_state, index, arg)?);
    } else {
      output.extend(extract_arg(generator_state, index, input_index)?);
      output.extend(from_arg(generator_state, index, arg)?);
      input_index += 1;
    }
  }

  output.extend(deferred);
  output.extend(call(generator_state)?);
  output.extend(return_value(generator_state, &signature.ret_val)?);

  let with_scope = if generator_state.needs_scope {
    with_scope(generator_state)
  } else {
    quote!()
  };

  let with_opctx = if generator_state.needs_opctx {
    with_opctx(generator_state)
  } else {
    quote!()
  };

  let with_retval = if generator_state.needs_retval {
    with_retval(generator_state)
  } else {
    quote!()
  };

  let with_args = if generator_state.needs_args {
    with_fn_args(generator_state)
  } else {
    quote!()
  };

  let GeneratorState {
    deno_core,
    info,
    slow_function,
    ..
  } = &generator_state;

  Ok(quote! {
    extern "C" fn #slow_function(#info: *const #deno_core::v8::FunctionCallbackInfo) {
    #with_scope
    #with_retval
    #with_args
    #with_opctx

    #output
  }})
}

fn with_scope(generator_state: &mut GeneratorState) -> TokenStream {
  let GeneratorState {
    deno_core,
    scope,
    info,
    ..
  } = &generator_state;

  quote!(let #scope = &mut unsafe { #deno_core::v8::CallbackScope::new(&*#info) };)
}

fn with_retval(generator_state: &mut GeneratorState) -> TokenStream {
  let GeneratorState {
    deno_core,
    retval,
    info,
    ..
  } = &generator_state;

  quote!(let mut #retval = #deno_core::v8::ReturnValue::from_function_callback_info(unsafe { &*#info });)
}

fn with_fn_args(generator_state: &mut GeneratorState) -> TokenStream {
  let GeneratorState {
    deno_core,
    fn_args,
    info,
    ..
  } = &generator_state;

  quote!(let #fn_args = #deno_core::v8::FunctionCallbackArguments::from_function_callback_info(unsafe { &*#info });)
}

fn with_opctx(generator_state: &mut GeneratorState) -> TokenStream {
  let GeneratorState {
    deno_core,
    opctx,
    fn_args,
    needs_args,
    ..
  } = generator_state;

  *needs_args = true;
  quote!(let #opctx = unsafe {
    &*(#deno_core::v8::Local::<#deno_core::v8::External>::cast(#fn_args.data()).value()
        as *const #deno_core::_ops::OpCtx)
  };)
}

pub fn extract_arg(
  generator_state: &mut GeneratorState,
  index: usize,
  input_index: usize,
) -> Result<TokenStream, V8MappingError> {
  let GeneratorState { fn_args, .. } = &generator_state;
  let arg_ident = generator_state.args.get(index);

  Ok(quote!(
    let #arg_ident = #fn_args.get(#input_index as i32);
  ))
}

pub fn from_arg(
  mut generator_state: &mut GeneratorState,
  index: usize,
  arg: &Arg,
) -> Result<TokenStream, V8MappingError> {
  let GeneratorState {
    deno_core,
    args,
    scope,
    needs_scope,
    ..
  } = &mut generator_state;
  let arg_ident = args.get_mut(index).expect("Argument at index was missing");
  let arg_temp = format_ident!("{}_temp", arg_ident);
  let res = match arg {
    Arg::Numeric(NumericArg::bool) => quote! {
      let #arg_ident = #arg_ident.is_true();
    },
    Arg::Numeric(NumericArg::u8)
    | Arg::Numeric(NumericArg::u16)
    | Arg::Numeric(NumericArg::u32) => {
      quote! {
        let #arg_ident = #deno_core::_ops::to_u32(&#arg_ident) as _;
      }
    }
    Arg::Numeric(NumericArg::i8)
    | Arg::Numeric(NumericArg::i16)
    | Arg::Numeric(NumericArg::i32)
    | Arg::Numeric(NumericArg::__SMI__) => {
      quote! {
        let #arg_ident = #deno_core::_ops::to_i32(&#arg_ident) as _;
      }
    }
    Arg::Numeric(NumericArg::u64) | Arg::Numeric(NumericArg::usize) => {
      quote! {
        let #arg_ident = #deno_core::_ops::to_u64(&#arg_ident) as _;
      }
    }
    Arg::Numeric(NumericArg::i64) | Arg::Numeric(NumericArg::isize) => {
      quote! {
        let #arg_ident = #deno_core::_ops::to_i64(&#arg_ident) as _;
      }
    }
    Arg::OptionNumeric(numeric) => {
      // Ends the borrow of generator_state
      let arg_ident = arg_ident.clone();
      let some = from_arg(generator_state, index, &Arg::Numeric(*numeric))?;
      quote! {
        let #arg_ident = if #arg_ident.is_null_or_undefined() {
          None
        } else {
          #some
          Some(#arg_ident)
        };
      }
    }
    Arg::Option(Special::String) => {
      *needs_scope = true;
      quote! {
        let #arg_ident = #arg_ident.to_rust_string_lossy(#scope);
      }
    }
    Arg::Special(Special::String) => {
      *needs_scope = true;
      quote! {
        let #arg_ident = #arg_ident.to_rust_string_lossy(#scope);
      }
    }
    Arg::Special(Special::RefStr) => {
      *needs_scope = true;
      quote! {
        // Trade 1024 bytes of stack space for potentially non-allocating strings
        let mut #arg_temp: [::std::mem::MaybeUninit<u8>; 1024] = [::std::mem::MaybeUninit::uninit(); 1024];
        let #arg_ident = &#deno_core::_ops::to_str(#scope, &#arg_ident, &mut #arg_temp);
      }
    }
    Arg::Special(Special::CowStr) => {
      *needs_scope = true;
      quote! {
        // Trade 1024 bytes of stack space for potentially non-allocating strings
        let mut #arg_temp: [::std::mem::MaybeUninit<u8>; 1024] = [::std::mem::MaybeUninit::uninit(); 1024];
        let #arg_ident = #deno_core::_ops::to_str(#scope, &#arg_ident, &mut #arg_temp);
      }
    }
    Arg::Ref(_, Special::HandleScope) => {
      *needs_scope = true;
      quote!(let #arg_ident = #scope;)
    }
    Arg::V8Ref(_, v8) => {
      let arg_ident = arg_ident.clone();
      let arg = from_v8_arg(generator_state, v8, &arg_ident)?;
      quote! {
        #arg;
        let #arg_ident = &#arg_ident;
      }
    }
    Arg::V8Local(v8) => {
      let arg_ident = arg_ident.clone();
      from_v8_arg(generator_state, v8, &arg_ident)?
    }
    Arg::OptionV8Ref(RefType::Ref, v8) => {
      let arg_ident = arg_ident.clone();
      let arg = from_arg(generator_state, index, &Arg::OptionV8Local(*v8))?;
      quote! {
        // First get the Option<Local>
        #arg;
        // Then map the reference
        let #arg_ident = #arg_ident.as_ref().map(|v| ::std::ops::Deref::deref(v));
      }
    }
    Arg::OptionV8Ref(RefType::Mut, v8) => {
      let arg_ident = arg_ident.clone();
      let arg = from_arg(generator_state, index, &Arg::OptionV8Local(*v8))?;
      quote! {
        // First get the Option<Local>
        #arg;
        // Then map the reference
        let #arg_ident = #arg_ident.as_mut().map(|v| ::std::ops::DerefMut::deref_mut(v));
      }
    }
    Arg::OptionV8Local(v8) => {
      let arg_ident = arg_ident.clone();
      let arg = from_v8_arg(generator_state, v8, &arg_ident)?;
      quote! {
        let #arg_ident = if #arg_ident.is_null_or_undefined() {
          None
        } else {
          #arg;
          Some(#arg_ident)
        };
      }
    }
    _ => return Err(V8MappingError::NoMapping("a slow argument", arg.clone())),
  };
  Ok(res)
}

/// Generates a v8::Value of the correct type for the required V8Arg, throwing an exception if the
/// type cannot be cast.
fn from_v8_arg(
  generator_state: &mut GeneratorState,
  v8: &V8Arg,
  arg_ident: &Ident,
) -> Result<TokenStream, V8MappingError> {
  let GeneratorState {
    deno_core,
    needs_scope,
    ..
  } = generator_state;

  *needs_scope = true;
  let v8: &'static str = v8.into();
  let deno_core = deno_core.clone();
  let throw_type_error =
    throw_type_error(generator_state, format!("expected v8::{}", v8))?;
  let v8 = format_ident!("{}", v8);
  Ok(quote! {
    let Ok(mut #arg_ident) = #deno_core::v8::Local::<#deno_core::v8::#v8>::try_from(#arg_ident) else {
      #throw_type_error
    };
  })
}

pub fn call(
  generator_state: &mut GeneratorState,
) -> Result<TokenStream, V8MappingError> {
  let GeneratorState { result, .. } = &generator_state;

  let mut tokens = TokenStream::new();
  for arg in &generator_state.args {
    tokens.extend(quote!( #arg , ));
  }
  Ok(quote! {
    let #result = Self::call( #tokens );
  })
}

pub fn return_value(
  generator_state: &mut GeneratorState,
  ret_type: &RetVal,
) -> Result<TokenStream, V8MappingError> {
  match ret_type {
    RetVal::Infallible(ret_type) => {
      return_value_infallible(generator_state, ret_type)
    }
    RetVal::Result(ret_type) => return_value_result(generator_state, ret_type),
  }
}

pub fn return_value_infallible(
  generator_state: &mut GeneratorState,
  ret_type: &Arg,
) -> Result<TokenStream, V8MappingError> {
  let GeneratorState {
    deno_core,
    scope,
    result,
    retval,
    needs_retval,
    needs_scope,
    ..
  } = generator_state;

  let res = match ret_type {
    Arg::Void => {
      quote! {/* void */}
    }
    Arg::Numeric(NumericArg::u8)
    | Arg::Numeric(NumericArg::u16)
    | Arg::Numeric(NumericArg::u32) => {
      *needs_retval = true;
      quote!(#retval.set_uint32(#result as u32);)
    }
    Arg::Numeric(NumericArg::i8)
    | Arg::Numeric(NumericArg::i16)
    | Arg::Numeric(NumericArg::i32) => {
      *needs_retval = true;
      quote!(#retval.set_int32(#result as i32);)
    }
    Arg::Special(Special::String) => {
      *needs_retval = true;
      *needs_scope = true;
      quote! {
        if #result.is_empty() {
          #retval.set_empty_string();
        } else {
          // This should not fail in normal cases
          // TODO(mmastrac): This has extra allocations that we need to get rid of, especially if the string
          // is ASCII. We could make an "external Rust String" string in V8 from these and re-use the allocation.
          let temp = #deno_core::v8::String::new(#scope, &#result).unwrap();
          #retval.set(temp.into());
        }
      }
    }
    Arg::Option(Special::String) => {
      *needs_retval = true;
      *needs_scope = true;
      // End the generator_state borrow
      let (result, retval) = (result.clone(), retval.clone());
      let some = return_value_infallible(
        generator_state,
        &Arg::Special(Special::String),
      )?;
      quote! {
        if let Some(#result) = #result {
          #some
        } else {
          #retval.set_null();
        }
      }
    }
    Arg::OptionV8Local(_) => {
      *needs_retval = true;
      quote! {
        if let Some(#result) = #result {
          // We may have a non v8::Value here
          #retval.set(#result.into())
        } else {
          #retval.set_null();
        }
      }
    }
    Arg::V8Local(_) => {
      *needs_retval = true;
      quote! {
        // We may have a non v8::Value here
        #retval.set(#result.into())
      }
    }
    _ => {
      return Err(V8MappingError::NoMapping(
        "a slow return value",
        ret_type.clone(),
      ))
    }
  };

  Ok(res)
}

fn return_value_result(
  generator_state: &mut GeneratorState,
  ret_type: &Arg,
) -> Result<TokenStream, V8MappingError> {
  let infallible = return_value_infallible(generator_state, ret_type)?;
  let exception = throw_exception(generator_state)?;

  let GeneratorState { result, .. } = &generator_state;

  let tokens = quote!(
    match #result {
      Ok(#result) => {
        #infallible
      }
      Err(err) => {
        #exception
      }
    };
  );
  Ok(tokens)
}

/// Generates code to throw an exception, adding required additional dependencies as needed.
fn throw_exception(
  generator_state: &mut GeneratorState,
) -> Result<TokenStream, V8MappingError> {
  let maybe_scope = if generator_state.needs_scope {
    quote!()
  } else {
    with_scope(generator_state)
  };

  let maybe_opctx = if generator_state.needs_opctx {
    quote!()
  } else {
    with_opctx(generator_state)
  };

  let maybe_args = if generator_state.needs_args {
    quote!()
  } else {
    with_fn_args(generator_state)
  };

  let GeneratorState {
    deno_core,
    scope,
    opctx,
    ..
  } = &generator_state;

  Ok(quote! {
    #maybe_scope
    #maybe_args
    #maybe_opctx
    let opstate = ::std::cell::RefCell::borrow(&*#opctx.state);
    let exception = #deno_core::error::to_v8_error(
      #scope,
      opstate.get_error_class_fn,
      &err,
    );
    #scope.throw_exception(exception);
    return;
  })
}

/// Generates code to throw an exception, adding required additional dependencies as needed.
fn throw_type_error(
  generator_state: &mut GeneratorState,
  message: String,
) -> Result<TokenStream, V8MappingError> {
  // Sanity check ASCII and a valid/reasonable message size
  debug_assert!(
    message.is_ascii()
      && message.len() < v8::String::max_length()
      && message.len() < 1024
  );

  let maybe_scope = if generator_state.needs_scope {
    quote!()
  } else {
    with_scope(generator_state)
  };

  let GeneratorState {
    deno_core, scope, ..
  } = &generator_state;

  Ok(quote! {
    #maybe_scope
    let msg = #deno_core::v8::String::new_from_one_byte(#scope, #message.as_bytes(), #deno_core::v8::NewStringType::Normal).unwrap();
    let exc = #deno_core::v8::Exception::error(#scope, msg);
    #scope.throw_exception(exc);
    return;
  })
}
