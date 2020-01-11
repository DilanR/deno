// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

use crate::es_isolate::EsIsolate;
use crate::isolate::Isolate;
use crate::isolate::PinnedBuf;
use crate::isolate::SHARED_RESPONSE_BUF_SIZE;

use rusty_v8 as v8;
use v8::InIsolate;

use libc::c_void;
use std::convert::TryFrom;
use std::option::Option;
use std::ptr;
use std::slice;

lazy_static! {
  pub static ref EXTERNAL_REFERENCES: v8::ExternalReferences =
    v8::ExternalReferences::new(&[
      v8::ExternalReference { function: print },
      v8::ExternalReference { function: recv },
      v8::ExternalReference { function: send },
      v8::ExternalReference {
        function: eval_context
      },
      v8::ExternalReference {
        function: error_to_json
      },
      v8::ExternalReference {
        getter: shared_getter
      },
      v8::ExternalReference {
        message: message_callback
      },
      v8::ExternalReference {
        function: queue_microtask
      },
    ]);
}

pub fn script_origin<'a>(
  s: &mut impl v8::ToLocal<'a>,
  resource_name: v8::Local<'a, v8::String>,
) -> v8::ScriptOrigin<'a> {
  let resource_line_offset = v8::Integer::new(s, 0);
  let resource_column_offset = v8::Integer::new(s, 0);
  let resource_is_shared_cross_origin = v8::Boolean::new(s, false);
  let script_id = v8::Integer::new(s, 123);
  let source_map_url = v8::String::new(s, "source_map_url").unwrap();
  let resource_is_opaque = v8::Boolean::new(s, true);
  let is_wasm = v8::Boolean::new(s, false);
  let is_module = v8::Boolean::new(s, false);
  v8::ScriptOrigin::new(
    resource_name.into(),
    resource_line_offset,
    resource_column_offset,
    resource_is_shared_cross_origin,
    script_id,
    source_map_url.into(),
    resource_is_opaque,
    is_wasm,
    is_module,
  )
}

pub fn module_origin<'a>(
  s: &mut impl v8::ToLocal<'a>,
  resource_name: v8::Local<'a, v8::String>,
) -> v8::ScriptOrigin<'a> {
  let resource_line_offset = v8::Integer::new(s, 0);
  let resource_column_offset = v8::Integer::new(s, 0);
  let resource_is_shared_cross_origin = v8::Boolean::new(s, false);
  let script_id = v8::Integer::new(s, 123);
  let source_map_url = v8::String::new(s, "source_map_url").unwrap();
  let resource_is_opaque = v8::Boolean::new(s, true);
  let is_wasm = v8::Boolean::new(s, false);
  let is_module = v8::Boolean::new(s, true);
  v8::ScriptOrigin::new(
    resource_name.into(),
    resource_line_offset,
    resource_column_offset,
    resource_is_shared_cross_origin,
    script_id,
    source_map_url.into(),
    resource_is_opaque,
    is_wasm,
    is_module,
  )
}

pub fn initialize_context<'a>(
  scope: &mut impl v8::ToLocal<'a>,
  mut context: v8::Local<v8::Context>,
) {
  context.enter();

  let global = context.global(scope);

  let deno_val = v8::Object::new(scope);

  global.set(
    context,
    v8::String::new(scope, "Deno").unwrap().into(),
    deno_val.into(),
  );

  let mut core_val = v8::Object::new(scope);

  deno_val.set(
    context,
    v8::String::new(scope, "core").unwrap().into(),
    core_val.into(),
  );

  let mut print_tmpl = v8::FunctionTemplate::new(scope, print);
  let print_val = print_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "print").unwrap().into(),
    print_val.into(),
  );

  let mut recv_tmpl = v8::FunctionTemplate::new(scope, recv);
  let recv_val = recv_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "recv").unwrap().into(),
    recv_val.into(),
  );

  let mut send_tmpl = v8::FunctionTemplate::new(scope, send);
  let send_val = send_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "send").unwrap().into(),
    send_val.into(),
  );

  let mut eval_context_tmpl = v8::FunctionTemplate::new(scope, eval_context);
  let eval_context_val =
    eval_context_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "evalContext").unwrap().into(),
    eval_context_val.into(),
  );

  let mut error_to_json_tmpl = v8::FunctionTemplate::new(scope, error_to_json);
  let error_to_json_val =
    error_to_json_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "errorToJSON").unwrap().into(),
    error_to_json_val.into(),
  );

  core_val.set_accessor(
    context,
    v8::String::new(scope, "shared").unwrap().into(),
    shared_getter,
  );

  // Direct bindings on `window`.
  let mut queue_microtask_tmpl =
    v8::FunctionTemplate::new(scope, queue_microtask);
  let queue_microtask_val =
    queue_microtask_tmpl.get_function(scope, context).unwrap();
  global.set(
    context,
    v8::String::new(scope, "queueMicrotask").unwrap().into(),
    queue_microtask_val.into(),
  );

  context.exit();
}

pub unsafe fn slice_to_uint8array<'sc>(
  deno_isolate: &mut Isolate,
  scope: &mut impl v8::ToLocal<'sc>,
  buf: &[u8],
) -> v8::Local<'sc, v8::Uint8Array> {
  if buf.is_empty() {
    let ab = v8::ArrayBuffer::new(scope, 0);
    return v8::Uint8Array::new(ab, 0, 0).expect("Failed to create UintArray8");
  }

  let buf_len = buf.len();
  let buf_ptr = buf.as_ptr();

  // To avoid excessively allocating new ArrayBuffers, we try to reuse a single
  // global ArrayBuffer. The caveat is that users must extract data from it
  // before the next tick. We only do this for ArrayBuffers less than 1024
  // bytes.
  let ab = if buf_len > SHARED_RESPONSE_BUF_SIZE {
    // Simple case. We allocate a new ArrayBuffer for this.
    v8::ArrayBuffer::new(scope, buf_len)
  } else if deno_isolate.shared_response_buf.is_empty() {
    let buf = v8::ArrayBuffer::new(scope, SHARED_RESPONSE_BUF_SIZE);
    deno_isolate.shared_response_buf.set(scope, buf);
    buf
  } else {
    deno_isolate.shared_response_buf.get(scope).unwrap()
  };

  let mut backing_store = ab.get_backing_store();
  let data = backing_store.data();
  let data: *mut u8 = data as *mut libc::c_void as *mut u8;
  std::ptr::copy_nonoverlapping(buf_ptr, data, buf_len);
  v8::Uint8Array::new(ab, 0, buf_len).expect("Failed to create UintArray8")
}

pub extern "C" fn host_import_module_dynamically_callback(
  context: v8::Local<v8::Context>,
  referrer: v8::Local<v8::ScriptOrModule>,
  specifier: v8::Local<v8::String>,
) -> *mut v8::Promise {
  let mut cbs = v8::CallbackScope::new(context);
  let mut hs = v8::EscapableHandleScope::new(cbs.enter());
  let scope = hs.enter();
  let isolate = scope.isolate();
  let deno_isolate: &mut EsIsolate =
    unsafe { &mut *(isolate.get_data(1) as *mut EsIsolate) };

  // NOTE(bartlomieju): will crash for non-UTF-8 specifier
  let specifier_str = specifier
    .to_string(scope)
    .unwrap()
    .to_rust_string_lossy(scope);
  let referrer_name = referrer.get_resource_name();
  let referrer_name_str = referrer_name
    .to_string(scope)
    .unwrap()
    .to_rust_string_lossy(scope);

  // TODO(ry) I'm not sure what HostDefinedOptions is for or if we're ever going
  // to use it. For now we check that it is not used. This check may need to be
  // changed in the future.
  let host_defined_options = referrer.get_host_defined_options();
  assert_eq!(host_defined_options.length(), 0);

  let mut resolver = v8::PromiseResolver::new(scope, context).unwrap();
  let promise = resolver.get_promise(scope);

  let mut resolver_handle = v8::Global::new();
  resolver_handle.set(scope, resolver);

  let import_id = deno_isolate.next_dyn_import_id;
  deno_isolate.next_dyn_import_id += 1;
  deno_isolate
    .dyn_import_map
    .insert(import_id, resolver_handle);

  deno_isolate.dyn_import_cb(&specifier_str, &referrer_name_str, import_id);

  &mut *scope.escape(promise)
}

pub extern "C" fn host_initialize_import_meta_object_callback(
  context: v8::Local<v8::Context>,
  module: v8::Local<v8::Module>,
  meta: v8::Local<v8::Object>,
) {
  let mut cbs = v8::CallbackScope::new(context);
  let mut hs = v8::HandleScope::new(cbs.enter());
  let scope = hs.enter();
  let isolate = scope.isolate();
  let deno_isolate: &mut EsIsolate =
    unsafe { &mut *(isolate.get_data(1) as *mut EsIsolate) };

  let id = module.get_identity_hash();
  assert_ne!(id, 0);

  let info = deno_isolate.modules.get_info(id).expect("Module not found");

  meta.create_data_property(
    context,
    v8::String::new(scope, "url").unwrap().into(),
    v8::String::new(scope, &info.name).unwrap().into(),
  );
  meta.create_data_property(
    context,
    v8::String::new(scope, "main").unwrap().into(),
    v8::Boolean::new(scope, info.main).into(),
  );
}

pub extern "C" fn message_callback(
  message: v8::Local<v8::Message>,
  _exception: v8::Local<v8::Value>,
) {
  let mut message: v8::Local<v8::Message> =
    unsafe { std::mem::transmute(message) };
  let isolate = message.get_isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  assert!(!deno_isolate.global_context.is_empty());
  let context = deno_isolate.global_context.get(scope).unwrap();

  // TerminateExecution was called
  if isolate.is_execution_terminating() {
    let u = v8::new_undefined(scope);
    deno_isolate.handle_exception(scope, context, u.into());
    return;
  }

  let json_str = deno_isolate.encode_message_as_json(scope, context, message);
  deno_isolate.last_exception = Some(json_str);
}

pub extern "C" fn promise_reject_callback(msg: v8::PromiseRejectMessage) {
  #[allow(mutable_transmutes)]
  let mut msg: v8::PromiseRejectMessage = unsafe { std::mem::transmute(msg) };
  let isolate = msg.isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  let mut locker = v8::Locker::new(isolate);
  assert!(!deno_isolate.global_context.is_empty());
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let mut context = deno_isolate.global_context.get(scope).unwrap();
  context.enter();

  let promise = msg.get_promise();
  let promise_id = promise.get_identity_hash();

  match msg.get_event() {
    v8::PromiseRejectEvent::PromiseRejectWithNoHandler => {
      let error = msg.get_value();
      let mut error_global = v8::Global::<v8::Value>::new();
      error_global.set(scope, error);
      deno_isolate
        .pending_promise_map
        .insert(promise_id, error_global);
    }
    v8::PromiseRejectEvent::PromiseHandlerAddedAfterReject => {
      if let Some(mut handle) =
        deno_isolate.pending_promise_map.remove(&promise_id)
      {
        handle.reset(scope);
      }
    }
    v8::PromiseRejectEvent::PromiseRejectAfterResolved => {}
    v8::PromiseRejectEvent::PromiseResolveAfterResolved => {
      // Should not warn. See #1272
    }
  };

  context.exit();
}

pub extern "C" fn print(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };

  let arg_len = info.length();
  assert!(arg_len >= 0 && arg_len <= 2);

  let obj = info.get_argument(0);
  let is_err_arg = info.get_argument(1);

  let mut hs = v8::HandleScope::new(info);
  let scope = hs.enter();

  let mut is_err = false;
  if arg_len == 2 {
    let int_val = is_err_arg
      .integer_value(scope)
      .expect("Unable to convert to integer");
    is_err = int_val != 0;
  };
  let mut try_catch = v8::TryCatch::new(scope);
  let _tc = try_catch.enter();
  let str_ = match obj.to_string(scope) {
    Some(s) => s,
    None => v8::String::new(scope, "").unwrap(),
  };
  if is_err {
    eprint!("{}", str_.to_rust_string_lossy(scope));
  } else {
    print!("{}", str_.to_rust_string_lossy(scope));
  }
}

pub extern "C" fn recv(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  assert_eq!(info.length(), 1);
  let isolate = info.get_isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  let mut locker = v8::Locker::new(&isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();

  if !deno_isolate.js_recv_cb.is_empty() {
    let msg = v8::String::new(scope, "Deno.core.recv already called.").unwrap();
    isolate.throw_exception(msg.into());
    return;
  }

  let recv_fn =
    v8::Local::<v8::Function>::try_from(info.get_argument(0)).unwrap();
  deno_isolate.js_recv_cb.set(scope, recv_fn);
}

pub extern "C" fn send(info: &v8::FunctionCallbackInfo) {
  let rv = &mut info.get_return_value();

  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };

  let arg0 = info.get_argument(0);
  let arg1 = info.get_argument(1);
  let arg2 = info.get_argument(2);
  let mut hs = v8::HandleScope::new(info);
  let scope = hs.enter();
  let isolate = scope.isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  assert!(!deno_isolate.global_context.is_empty());

  let op_id = v8::Local::<v8::Uint32>::try_from(arg0).unwrap().value() as u32;

  let control = match v8::Local::<v8::ArrayBufferView>::try_from(arg1) {
    Ok(view) => {
      let mut backing_store = view.buffer().unwrap().get_backing_store();
      let backing_store_ptr = backing_store.data() as *mut _ as *mut u8;
      let view_ptr = unsafe { backing_store_ptr.add(view.byte_offset()) };
      let view_len = view.byte_length();
      unsafe { slice::from_raw_parts(view_ptr, view_len) }
    }
    Err(..) => unsafe { slice::from_raw_parts(ptr::null(), 0) },
  };

  let zero_copy: Option<PinnedBuf> =
    v8::Local::<v8::ArrayBufferView>::try_from(arg2)
      .map(PinnedBuf::new)
      .ok();

  // If response is empty then it's either async op or exception was thrown
  let maybe_response = deno_isolate.dispatch_op(op_id, control, zero_copy);

  if let Some(response) = maybe_response {
    // Synchronous response.
    // Note op_id is not passed back in the case of synchronous response.
    let (_op_id, buf) = response;

    if !buf.is_empty() {
      let ab = unsafe { slice_to_uint8array(deno_isolate, scope, &buf) };
      rv.set(ab.into())
    }
  }
}

pub extern "C" fn eval_context(info: &v8::FunctionCallbackInfo) {
  let rv = &mut info.get_return_value();

  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  let arg0 = info.get_argument(0);

  let mut hs = v8::HandleScope::new(info);
  let scope = hs.enter();
  let isolate = scope.isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  assert!(!deno_isolate.global_context.is_empty());
  let context = deno_isolate.global_context.get(scope).unwrap();

  let source = match v8::Local::<v8::String>::try_from(arg0) {
    Ok(s) => s,
    Err(_) => {
      let msg = v8::String::new(scope, "Invalid argument").unwrap();
      let exception = v8::type_error(scope, msg);
      scope.isolate().throw_exception(exception);
      return;
    }
  };

  let output = v8::Array::new(scope, 2);
  /*
   output[0] = result
   output[1] = ErrorInfo | null
     ErrorInfo = {
       thrown: Error | any,
       isNativeError: boolean,
       isCompileError: boolean,
     }
  */
  let mut try_catch = v8::TryCatch::new(scope);
  let tc = try_catch.enter();
  let name = v8::String::new(scope, "<unknown>").unwrap();
  let origin = script_origin(scope, name);
  let maybe_script = v8::Script::compile(scope, context, source, Some(&origin));

  if maybe_script.is_none() {
    assert!(tc.has_caught());
    let exception = tc.exception().unwrap();

    output.set(
      context,
      v8::Integer::new(scope, 0).into(),
      v8::new_null(scope).into(),
    );

    let errinfo_obj = v8::Object::new(scope);
    errinfo_obj.set(
      context,
      v8::String::new(scope, "isCompileError").unwrap().into(),
      v8::Boolean::new(scope, true).into(),
    );

    errinfo_obj.set(
      context,
      v8::String::new(scope, "isNativeError").unwrap().into(),
      v8::Boolean::new(scope, exception.is_native_error()).into(),
    );

    errinfo_obj.set(
      context,
      v8::String::new(scope, "thrown").unwrap().into(),
      exception,
    );

    output.set(
      context,
      v8::Integer::new(scope, 1).into(),
      errinfo_obj.into(),
    );

    rv.set(output.into());
    return;
  }

  let result = maybe_script.unwrap().run(scope, context);

  if result.is_none() {
    assert!(tc.has_caught());
    let exception = tc.exception().unwrap();

    output.set(
      context,
      v8::Integer::new(scope, 0).into(),
      v8::new_null(scope).into(),
    );

    let errinfo_obj = v8::Object::new(scope);
    errinfo_obj.set(
      context,
      v8::String::new(scope, "isCompileError").unwrap().into(),
      v8::Boolean::new(scope, false).into(),
    );

    let is_native_error = if exception.is_native_error() {
      v8::Boolean::new(scope, true)
    } else {
      v8::Boolean::new(scope, false)
    };

    errinfo_obj.set(
      context,
      v8::String::new(scope, "isNativeError").unwrap().into(),
      is_native_error.into(),
    );

    errinfo_obj.set(
      context,
      v8::String::new(scope, "thrown").unwrap().into(),
      exception,
    );

    output.set(
      context,
      v8::Integer::new(scope, 1).into(),
      errinfo_obj.into(),
    );

    rv.set(output.into());
    return;
  }

  output.set(context, v8::Integer::new(scope, 0).into(), result.unwrap());
  output.set(
    context,
    v8::Integer::new(scope, 1).into(),
    v8::new_null(scope).into(),
  );
  rv.set(output.into());
}

pub extern "C" fn error_to_json(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  assert_eq!(info.length(), 1);
  // <Boilerplate>
  let isolate = info.get_isolate();
  let deno_isolate: &mut Isolate =
    unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };
  let mut locker = v8::Locker::new(&isolate);
  assert!(!deno_isolate.global_context.is_empty());
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let context = deno_isolate.global_context.get(scope).unwrap();
  // </Boilerplate>
  let exception = info.get_argument(0);
  let json_string =
    deno_isolate.encode_exception_as_json(scope, context, exception);
  let s = v8::String::new(scope, &json_string).unwrap();
  let mut rv = info.get_return_value();
  rv.set(s.into());
}

pub extern "C" fn queue_microtask(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  assert_eq!(info.length(), 1);
  let arg0 = info.get_argument(0);
  let isolate = info.get_isolate();
  let mut locker = v8::Locker::new(&isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();

  match v8::Local::<v8::Function>::try_from(arg0) {
    Ok(f) => isolate.enqueue_microtask(f),
    Err(_) => {
      let msg = v8::String::new(scope, "Invalid argument").unwrap();
      let exception = v8::type_error(scope, msg);
      isolate.throw_exception(exception);
    }
  };
}

pub extern "C" fn shared_getter(
  _name: v8::Local<v8::Name>,
  info: &v8::PropertyCallbackInfo,
) {
  let shared_ab = {
    #[allow(mutable_transmutes)]
    #[allow(clippy::transmute_ptr_to_ptr)]
    let info: &mut v8::PropertyCallbackInfo =
      unsafe { std::mem::transmute(info) };

    let mut hs = v8::EscapableHandleScope::new(info);
    let scope = hs.enter();
    let isolate = scope.isolate();
    let deno_isolate: &mut Isolate =
      unsafe { &mut *(isolate.get_data(0) as *mut Isolate) };

    // Lazily initialize the persistent external ArrayBuffer.
    if deno_isolate.shared_ab.is_empty() {
      let data_ptr = deno_isolate.shared.bytes.as_ptr();
      let data_len = deno_isolate.shared.bytes.len();
      let ab = unsafe {
        v8::SharedArrayBuffer::new_DEPRECATED(
          scope,
          data_ptr as *mut c_void,
          data_len,
        )
      };
      deno_isolate.shared_ab.set(scope, ab);
    }

    let shared_ab = deno_isolate.shared_ab.get(scope).unwrap();
    scope.escape(shared_ab)
  };

  let rv = &mut info.get_return_value();
  rv.set(shared_ab.into());
}

pub fn module_resolve_callback(
  context: v8::Local<v8::Context>,
  specifier: v8::Local<v8::String>,
  referrer: v8::Local<v8::Module>,
) -> *mut v8::Module {
  let mut cbs = v8::CallbackScope::new(context);
  let cb_scope = cbs.enter();
  let isolate = cb_scope.isolate();
  let deno_isolate: &mut EsIsolate =
    unsafe { &mut *(isolate.get_data(1) as *mut EsIsolate) };

  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::EscapableHandleScope::new(&mut locker);
  let scope = hs.enter();

  let referrer_id = referrer.get_identity_hash();
  let referrer_name = deno_isolate
    .modules
    .get_info(referrer_id)
    .expect("ModuleInfo not found")
    .name
    .to_string();
  let len_ = referrer.get_module_requests_length();

  let specifier_str = specifier.to_rust_string_lossy(scope);

  for i in 0..len_ {
    let req = referrer.get_module_request(i);
    let req_str = req.to_rust_string_lossy(scope);

    if req_str == specifier_str {
      let id = deno_isolate.module_resolve_cb(&req_str, referrer_id);
      let maybe_info = deno_isolate.modules.get_info(id);

      if maybe_info.is_none() {
        let msg = format!(
          "Cannot resolve module \"{}\" from \"{}\"",
          req_str, referrer_name
        );
        let msg = v8::String::new(scope, &msg).unwrap();
        isolate.throw_exception(msg.into());
        break;
      }

      let child_mod =
        maybe_info.unwrap().handle.get(scope).expect("Empty handle");
      return &mut *scope.escape(child_mod);
    }
  }

  std::ptr::null_mut()
}

pub fn encode_message_as_object<'a>(
  s: &mut impl v8::ToLocal<'a>,
  context: v8::Local<v8::Context>,
  message: v8::Local<v8::Message>,
) -> v8::Local<'a, v8::Object> {
  let json_obj = v8::Object::new(s);

  let exception_str = message.get(s);
  json_obj.set(
    context,
    v8::String::new(s, "message").unwrap().into(),
    exception_str.into(),
  );

  let script_resource_name = message
    .get_script_resource_name(s)
    .expect("Missing ScriptResourceName");
  json_obj.set(
    context,
    v8::String::new(s, "scriptResourceName").unwrap().into(),
    script_resource_name,
  );

  let source_line = message
    .get_source_line(s, context)
    .expect("Missing SourceLine");
  json_obj.set(
    context,
    v8::String::new(s, "sourceLine").unwrap().into(),
    source_line.into(),
  );

  let line_number = message
    .get_line_number(context)
    .expect("Missing LineNumber");
  json_obj.set(
    context,
    v8::String::new(s, "lineNumber").unwrap().into(),
    v8::Integer::new(s, line_number as i32).into(),
  );

  json_obj.set(
    context,
    v8::String::new(s, "startPosition").unwrap().into(),
    v8::Integer::new(s, message.get_start_position() as i32).into(),
  );

  json_obj.set(
    context,
    v8::String::new(s, "endPosition").unwrap().into(),
    v8::Integer::new(s, message.get_end_position() as i32).into(),
  );

  json_obj.set(
    context,
    v8::String::new(s, "errorLevel").unwrap().into(),
    v8::Integer::new(s, message.error_level() as i32).into(),
  );

  json_obj.set(
    context,
    v8::String::new(s, "startColumn").unwrap().into(),
    v8::Integer::new(s, message.get_start_column() as i32).into(),
  );

  json_obj.set(
    context,
    v8::String::new(s, "endColumn").unwrap().into(),
    v8::Integer::new(s, message.get_end_column() as i32).into(),
  );

  let is_shared_cross_origin =
    v8::Boolean::new(s, message.is_shared_cross_origin());

  json_obj.set(
    context,
    v8::String::new(s, "isSharedCrossOrigin").unwrap().into(),
    is_shared_cross_origin.into(),
  );

  let is_opaque = v8::Boolean::new(s, message.is_opaque());

  json_obj.set(
    context,
    v8::String::new(s, "isOpaque").unwrap().into(),
    is_opaque.into(),
  );

  let frames = if let Some(stack_trace) = message.get_stack_trace(s) {
    let count = stack_trace.get_frame_count() as i32;
    let frames = v8::Array::new(s, count);

    for i in 0..count {
      let frame = stack_trace
        .get_frame(s, i as usize)
        .expect("No frame found");
      let frame_obj = v8::Object::new(s);
      frames.set(context, v8::Integer::new(s, i).into(), frame_obj.into());
      frame_obj.set(
        context,
        v8::String::new(s, "line").unwrap().into(),
        v8::Integer::new(s, frame.get_line_number() as i32).into(),
      );
      frame_obj.set(
        context,
        v8::String::new(s, "column").unwrap().into(),
        v8::Integer::new(s, frame.get_column() as i32).into(),
      );

      if let Some(function_name) = frame.get_function_name(s) {
        frame_obj.set(
          context,
          v8::String::new(s, "functionName").unwrap().into(),
          function_name.into(),
        );
      }

      let script_name = match frame.get_script_name_or_source_url(s) {
        Some(name) => name,
        None => v8::String::new(s, "<unknown>").unwrap(),
      };
      frame_obj.set(
        context,
        v8::String::new(s, "scriptName").unwrap().into(),
        script_name.into(),
      );

      frame_obj.set(
        context,
        v8::String::new(s, "isEval").unwrap().into(),
        v8::Boolean::new(s, frame.is_eval()).into(),
      );

      frame_obj.set(
        context,
        v8::String::new(s, "isConstructor").unwrap().into(),
        v8::Boolean::new(s, frame.is_constructor()).into(),
      );

      frame_obj.set(
        context,
        v8::String::new(s, "isWasm").unwrap().into(),
        v8::Boolean::new(s, frame.is_wasm()).into(),
      );
    }

    frames
  } else {
    // No stack trace. We only have one stack frame of info..
    let frames = v8::Array::new(s, 1);
    let frame_obj = v8::Object::new(s);
    frames.set(context, v8::Integer::new(s, 0).into(), frame_obj.into());

    frame_obj.set(
      context,
      v8::String::new(s, "scriptResourceName").unwrap().into(),
      script_resource_name,
    );
    frame_obj.set(
      context,
      v8::String::new(s, "line").unwrap().into(),
      v8::Integer::new(s, line_number as i32).into(),
    );
    frame_obj.set(
      context,
      v8::String::new(s, "column").unwrap().into(),
      v8::Integer::new(s, message.get_start_column() as i32).into(),
    );

    frames
  };

  json_obj.set(
    context,
    v8::String::new(s, "frames").unwrap().into(),
    frames.into(),
  );

  json_obj
}
