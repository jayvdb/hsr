#![recursion_limit = "256"]
#![allow(unused_imports, dead_code)]

use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use actix_http::http::StatusCode;
use derive_more::{Deref, Display, From};
use either::Either;
use heck::{CamelCase, MixedCase, SnakeCase};
use indexmap::{IndexMap, IndexSet as Set};
use log::{debug, info};
use openapiv3::{
    AnySchema, ObjectType, OpenAPI, ReferenceOr, Schema, SchemaData, SchemaKind,
    StatusCode as ApiStatusCode, Type as ApiType,
};
use proc_macro2::{Ident as QIdent, TokenStream};
use quote::quote;
use regex::Regex;
use thiserror::Error;

mod route;
mod walk;

use route::Route;

const SWAGGER_UI_TEMPLATE: &'static str = include_str!("../ui-template.html");

fn ident(s: impl fmt::Display) -> QIdent {
    QIdent::new(&s.to_string(), proc_macro2::Span::call_site())
}

type Map<T> = IndexMap<String, T>;
type IdMap<T> = IndexMap<Ident, T>;
type TypeMap<T> = IndexMap<TypeName, T>;
type SchemaLookup = IndexMap<String, ReferenceOr<Schema>>;
type ResponseLookup = IndexMap<String, ReferenceOr<openapiv3::Response>>;
type ParametersLookup = IndexMap<String, ReferenceOr<openapiv3::Parameter>>;
type RequestLookup = IndexMap<String, ReferenceOr<openapiv3::RequestBody>>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO Error: {}", _0)]
    Io(#[from] std::io::Error),
    #[error("Yaml Error: {}", _0)]
    Yaml(#[from] serde_yaml::Error),
    #[error("Codegen failed")]
    CodeGen,
    #[error("Bad reference: \"{}\"", _0)]
    BadReference(String),
    #[error("Invalid schema: \"{}\"", _0)]
    BadSchema(String),
    #[error("Unexpected reference: \"{}\"", _0)]
    UnexpectedReference(String),
    #[error("Schema not supported: {:?}", _0)]
    UnsupportedKind(SchemaKind),
    #[error("Definition is too complex: {:?}", _0)]
    TooComplex(Schema),
    #[error("Empty struct")]
    EmptyStruct,
    #[error("Rust does not support structural typing")]
    NotStructurallyTyped,
    #[error("Path is malformed: {}", _0)]
    MalformedPath(String),
    #[error("No operatio Join Private Q&A n id given for route {}", _0)]
    NoOperationId(String),
    #[error("TODO: {}", _0)]
    Todo(String),
    #[error("{} is not a valid identifier", _0)]
    BadIdentifier(String),
    #[error("{} is not a valid type name", _0)]
    BadTypeName(String),
    #[error("Malformed codegen")]
    BadCodegen,
    #[error("status code '{}' not supported", _0)]
    BadStatusCode(ApiStatusCode),
    #[error("Duplicate name: {}", _0)]
    DuplicateName(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Unwrap the reference, or fail
fn unwrap_ref<T>(item: &ReferenceOr<T>) -> Result<&T> {
    match item {
        ReferenceOr::Item(item) => Ok(item),
        ReferenceOr::Reference { reference } => {
            Err(Error::UnexpectedReference(reference.to_string()))
        }
    }
}

/// Fetch reference target via a lookup
fn dereference<'a, T>(refr: &'a ReferenceOr<T>, lookup: &'a Map<ReferenceOr<T>>) -> Result<&'a T> {
    match refr {
        ReferenceOr::Reference { reference } => lookup
            .get(reference)
            .ok_or_else(|| Error::BadReference(reference.to_string()))
            .and_then(|refr| dereference(refr, lookup)),
        ReferenceOr::Item(item) => Ok(item),
    }
}

/// Check that a reference string exists within the API
fn validate_schema_ref<'a>(
    refr: &'a str,
    schema_lookup: &'a SchemaLookup,
) -> Result<(TypeName, SchemaData)> {
    fn validate_schema_ref_depth<'a>(
        refr: &'a str,
        lookup: &'a SchemaLookup,
        depth: u32,
    ) -> Result<(TypeName, SchemaData)> {
        if depth > 20 {
            // circular reference!!
            return Err(Error::BadReference(refr.to_string()));
        }
        let name = extract_ref_name(refr)?;
        // Do the lookup. We are just checking the ref points to 'something'
        let schema_or_ref = lookup
            .get(&name.to_string())
            .ok_or(Error::BadReference(refr.to_string()))?;
        // recursively dereference
        match schema_or_ref {
            ReferenceOr::Reference { reference } => {
                let (_, meta) = validate_schema_ref_depth(reference, lookup, depth + 1)?;
                Ok((name, meta))
            }
            ReferenceOr::Item(schema) => Ok((name, schema.schema_data.clone())),
        }
    }
    validate_schema_ref_depth(refr, schema_lookup, 0)
}

/// Extract the type name from the reference
fn extract_ref_name(refr: &str) -> Result<TypeName> {
    // This only works with form '#/components/schema/<NAME>'
    let err = Error::BadReference(refr.to_string());
    if !refr.starts_with("#") {
        return Err(err);
    }
    let parts: Vec<&str> = refr.split('/').collect();
    if !(parts.len() == 4 && parts[1] == "components" && parts[2] == "schemas") {
        return Err(err);
    }
    TypeName::new(parts[3].to_string())
}

/// Traverse the '#/components/schema' section of the spec and prepare type
/// definitions from the descriptions
fn gather_component_types(schema_lookup: &SchemaLookup) -> Result<TypeMap<StructOrType>> {
    let mut typs = TypeMap::new();
    // gather types defined in components
    for (name, schema) in schema_lookup {
        info!("Processing schema: {}", name);
        let typename = TypeName::new(name.clone())?;
        let typ = build_type(&schema, &schema_lookup)?;
        assert!(typs.insert(typename, typ).is_none());
    }
    Ok(typs)
}

fn api_trait_name(api: &OpenAPI) -> TypeName {
    TypeName::new(format!("{}Api", api.info.title.to_camel_case())).unwrap()
}

/// Separately represents methods which CANNOT take a body (GET, HEAD, OPTIONS, TRACE)
/// and those which MAY take a body (POST, PATCH, PUT, DELETE)
#[derive(Debug, Clone)]
enum Method {
    WithoutBody(MethodWithoutBody),
    WithBody {
        method: MethodWithBody,
        /// The expected body payload, if any
        body_type: Option<Type>,
    },
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::WithoutBody(method) => method.fmt(f),
            Self::WithBody { method, .. } => method.fmt(f),
        }
    }
}

impl Method {
    fn body_type(&self) -> Option<&Type> {
        match self {
            Method::WithoutBody(_)
            | Method::WithBody {
                body_type: None, ..
            } => None,
            Method::WithBody {
                body_type: Some(ref body_ty),
                ..
            } => Some(body_ty),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MethodWithoutBody {
    Get,
    Head,
    Options,
    Trace,
}

impl fmt::Display for MethodWithoutBody {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use MethodWithoutBody::*;
        match self {
            Get => write!(f, "GET"),
            Head => write!(f, "HEAD"),
            Options => write!(f, "OPTIONS"),
            Trace => write!(f, "TRACE"),
        }
    }
}

impl fmt::Display for MethodWithBody {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use MethodWithBody::*;
        match self {
            Post => write!(f, "POST"),
            Delete => write!(f, "DELETE"),
            Put => write!(f, "PUT"),
            Patch => write!(f, "PATCH"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MethodWithBody {
    Post,
    Delete,
    Put,
    Patch,
}

/// Build hsr Type from OpenAPI Response
fn get_type_of_response(
    ref_or_resp: &ReferenceOr<openapiv3::Response>,
    response_lookup: &ResponseLookup,
    schema_lookup: &SchemaLookup,
) -> Result<Option<Type>> {
    let resp = dereference(ref_or_resp, response_lookup)?;
    if !resp.headers.is_empty() {
        todo!("response headers not supported")
    }
    if !resp.links.is_empty() {
        todo!("response links not supported")
    }
    if resp.content.is_empty() {
        Ok(None)
    } else if !(resp.content.len() == 1 && resp.content.contains_key("application/json")) {
        todo!("content type must be 'application/json'")
    } else {
        let ref_or_schema = resp
            .content
            .get("application/json")
            .unwrap()
            .schema
            .as_ref()
            .ok_or_else(|| todo!("Media type does not contain schema"))
            .unwrap();
        Ok(Some(
            build_type(&ref_or_schema, schema_lookup).and_then(|s| s.discard_struct())?,
        ))
    }
}

/// A string which is a valid identifier (snake_case)
///
/// Do not construct directly, instead use str.parse
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Deref)]
struct Ident(String);

impl FromStr for Ident {
    type Err = Error;
    fn from_str(val: &str) -> Result<Self> {
        // Check the identifier string in valid.
        // We use snake_case internally, but additionally allow mixedCase identifiers
        // to be passed, since this is common in JS world
        let snake = val.to_snake_case();
        let mixed = val.to_mixed_case();
        if val == snake || val == mixed {
            Ok(Ident(snake))
        } else {
            Err(Error::BadIdentifier(val.to_string()))
        }
    }
}

impl quote::ToTokens for Ident {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let id = ident(&self.0);
        id.to_tokens(tokens)
    }
}

/// A string which is a valid name for type (CamelCase)
///
/// Do not construct directly, instead use `new`
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, Deref)]
struct TypeName(String);

impl TypeName {
    fn new(val: String) -> Result<Self> {
        if val == val.to_camel_case() {
            Ok(TypeName(val))
        } else {
            Err(Error::BadTypeName(val))
        }
    }
}

impl quote::ToTokens for TypeName {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let id = ident(&self.0);
        id.to_tokens(tokens)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSegment {
    Literal(String),
    Parameter(String),
}

#[derive(Clone, Debug)]
struct RoutePath {
    segments: Vec<PathSegment>,
}

impl fmt::Display for RoutePath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut path = String::new();
        for segment in &self.segments {
            match segment {
                PathSegment::Literal(p) => {
                    path.push('/');
                    path.push_str(&p);
                }
                PathSegment::Parameter(_) => {
                    path.push_str("/{}");
                }
            }
        }
        write!(f, "{}", path)
    }
}

impl RoutePath {
    /// Check a path is well-formed and break it into its respective `PathSegment`s
    fn analyse(path: &str) -> Result<RoutePath> {
        // TODO lazy static
        let literal_re = Regex::new("^[[:alpha:]]+$").unwrap();
        let param_re = Regex::new(r#"^\{([[:alpha:]]+)\}$"#).unwrap();

        if path.is_empty() || !path.starts_with('/') {
            return Err(Error::MalformedPath(path.to_string()));
        }

        let mut segments = Vec::new();

        for segment in path.split('/').skip(1) {
            // ignore trailing slashes
            if segment.is_empty() {
                continue;
            }
            if literal_re.is_match(segment) {
                segments.push(PathSegment::Literal(segment.to_string()))
            } else if let Some(seg) = param_re.captures(segment) {
                segments.push(PathSegment::Parameter(
                    seg.get(1).unwrap().as_str().to_string(),
                ))
            } else {
                return Err(Error::MalformedPath(path.to_string()));
            }
        }
        // TODO check for duplicate parameter names
        Ok(RoutePath { segments })
    }

    fn path_args(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| {
            if let PathSegment::Parameter(ref s) = s {
                Some(s.as_str())
            } else {
                None
            }
        })
    }

    fn build_template(&self) -> String {
        let mut path = String::new();
        for segment in &self.segments {
            match segment {
                PathSegment::Literal(p) => {
                    path.push('/');
                    path.push_str(&p);
                }
                PathSegment::Parameter(_) => {
                    path.push_str("/{}");
                }
            }
        }
        path
    }
}

fn gather_routes(
    paths: &openapiv3::Paths,
    schema_lookup: &SchemaLookup,
    response_lookup: &ResponseLookup,
    param_lookup: &ParametersLookup,
    req_body_lookup: &RequestLookup,
) -> Result<Map<Vec<Route>>> {
    let mut routes = Map::new();
    debug!("Found paths: {:?}", paths.keys().collect::<Vec<_>>());
    for (path, pathitem) in paths {
        debug!("Processing path: {:?}", path);
        let pathitem = unwrap_ref(&pathitem)?;
        let mut pathroutes = Vec::new();

        // *** methods without body ***
        // GET
        if let Some(ref op) = pathitem.get {
            let route = Route::without_body(
                path,
                MethodWithoutBody::Get,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        // OPTIONS
        if let Some(ref op) = pathitem.options {
            let route = Route::without_body(
                path,
                MethodWithoutBody::Options,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        // HEAD
        if let Some(ref op) = pathitem.head {
            let route = Route::without_body(
                path,
                MethodWithoutBody::Head,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        // TRACE
        if let Some(ref op) = pathitem.trace {
            let route = Route::without_body(
                path,
                MethodWithoutBody::Trace,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        // *** methods with body ***
        // POST
        if let Some(ref op) = pathitem.post {
            let route = Route::with_body(
                path,
                MethodWithBody::Post,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
                req_body_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        // PUT
        if let Some(ref op) = pathitem.put {
            let route = Route::with_body(
                path,
                MethodWithBody::Put,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
                req_body_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        if let Some(ref op) = pathitem.patch {
            let route = Route::with_body(
                path,
                MethodWithBody::Patch,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
                req_body_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        if let Some(ref op) = pathitem.delete {
            let route = Route::with_body(
                path,
                MethodWithBody::Delete,
                op,
                schema_lookup,
                response_lookup,
                param_lookup,
                req_body_lookup,
            )?;
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }

        let is_duped_key = routes.insert(path.to_string(), pathroutes).is_some();
        assert!(!is_duped_key);
    }
    Ok(routes)
}

#[derive(Debug, Clone, PartialEq)]
struct TypeWithMeta<T> {
    meta: SchemaData,
    typ: T,
}

impl StructOrType {
    fn discard_struct(self) -> Result<Type> {
        match self.typ {
            Either::Right(typ) => Ok(TypeWithMeta {
                meta: self.meta,
                typ,
            }),
            Either::Left(_) => return Err(Error::NotStructurallyTyped),
        }
    }
}

impl Type {
    fn to_option(self) -> Self {
        Self {
            meta: self.meta.clone(),
            typ: TypeInner::Option(Box::new(self)),
        }
    }
}

// Out general strategy is to recursively traverse the openapi object and gather all the
// types together. We separate out the types into Struct and TypeInner. A Struct represents
// a 'raw', unnamed object. It just informs us about the fields within. A bare Struct may not
// be instantiated directly, because it doesn't have a name.
type Type = TypeWithMeta<TypeInner>;
type StructOrType = TypeWithMeta<Either<Struct, TypeInner>>;

#[derive(Debug, Clone, PartialEq)]
enum TypeInner {
    // primitives
    String,
    F64,
    I64,
    Bool,
    // An array of of some inner type
    Array(Box<Type>),
    // A type which is nullable
    Option(Box<Type>),
    // Any type. Could be anything! Probably a user-error
    Any,
    // Some type which is defined elsewhere, we only have the name.
    // Probably comes from a reference
    Named(TypeName),
}

impl TypeInner {
    /// Attach metadata to
    fn with_meta_either(self, meta: SchemaData) -> StructOrType {
        TypeWithMeta {
            meta,
            typ: Either::Right(self),
        }
    }
}

impl quote::ToTokens for Type {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        self.typ.to_tokens(tokens)
    }
}

impl quote::ToTokens for TypeInner {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        use TypeInner::*;
        let toks = match self {
            String => quote! { String },
            F64 => quote! { f64 },
            I64 => quote! { i64 },
            Bool => quote! { bool },
            Array(inner) => {
                quote! { Vec<#inner> }
            }
            Option(inner) => {
                quote! { Option<#inner> }
            }
            Named(name) => {
                quote! { #name }
            }
            // TODO handle Any properly
            Any => unimplemented!(),
        };
        toks.to_tokens(tokens);
    }
}

impl Struct {
    fn with_meta_either(self, meta: SchemaData) -> StructOrType {
        TypeWithMeta {
            meta,
            typ: Either::Left(self),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Struct {
    fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
struct Field {
    name: Ident,
    ty: Type,
}

impl Struct {
    fn new(fields: Vec<Field>) -> Result<Struct> {
        if fields.is_empty() {
            return Err(Error::EmptyStruct);
        }
        // TODO other validation?
        Ok(Struct { fields })
    }

    fn from_objlike<T: ObjectLike>(obj: &T, schema_lookup: &SchemaLookup) -> Result<Self> {
        let mut fields = Vec::new();
        let required_args: Set<String> = obj.required().iter().cloned().collect();
        for (name, schemaref) in obj.properties() {
            let schemaref = schemaref.clone().unbox();
            let mut ty = build_type(&schemaref, schema_lookup).and_then(|s| s.discard_struct())?;
            if !required_args.contains(name) {
                ty = ty.to_option();
            }
            let field = Field {
                name: name.parse()?,
                ty,
            };
            fields.push(field);
        }
        Self::new(fields)
    }
}

trait ObjectLike {
    fn properties(&self) -> &Map<ReferenceOr<Box<Schema>>>;
    fn required(&self) -> &[String];
}

macro_rules! impl_objlike {
    ($obj:ty) => {
        impl ObjectLike for $obj {
            fn properties(&self) -> &Map<ReferenceOr<Box<Schema>>> {
                &self.properties
            }
            fn required(&self) -> &[String] {
                &self.required
            }
        }
    };
}

impl_objlike!(ObjectType);
impl_objlike!(AnySchema);

fn error_variant_from_status_code(code: &StatusCode) -> TypeName {
    code.canonical_reason()
        .and_then(|reason| TypeName::new(reason.to_camel_case()).ok())
        .unwrap_or(TypeName::new(format!("E{}", code.as_str())).unwrap())
}

fn doc_comment(msg: impl AsRef<str>) -> TokenStream {
    let msg = msg.as_ref();
    quote! {
        #[doc = #msg]
    }
}

fn get_derive_tokens() -> TokenStream {
    quote! {
        # [derive(Debug, Clone, PartialEq, PartialOrd, hsr::Serialize, hsr::Deserialize)]
    }
}

enum Visibility {
    Private,
    Public,
}

impl quote::ToTokens for Visibility {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            Visibility::Public => {
                let q = quote! { pub };
                q.to_tokens(tokens);
            }
            Visibility::Private => (),
        }
    }
}

/// Generate code that defines a `struct` or `type` alias for each object described
/// in the OpenAPI 'components' section.
fn generate_rust_component_types(typs: &TypeMap<StructOrType>) -> TokenStream {
    let mut tokens = TokenStream::new();
    for (typename, typ) in typs {
        let descr = typ.meta.description.as_ref().map(|s| s.as_str());
        let def = match &typ.typ {
            Either::Left(strukt) => {
                generate_struct_def(typename, descr, strukt, Visibility::Public)
            }
            Either::Right(typ) => {
                let descr = descr.map(doc_comment);
                // make a type alias
                quote! {
                    #descr
                    pub type #typename = #typ;
                }
            }
        };
        tokens.extend(def);
    }
    tokens
}

fn generate_rust_route_types(routemap: &Map<Vec<Route>>) -> TokenStream {
    let mut tokens = TokenStream::new();
    for (_path, routes) in routemap {
        for route in routes {
            // Construct the error type, if necessary
            let mb_enum_def = route.generate_error_enum_def();
            tokens.extend(mb_enum_def);
            // construct the query type, if necessary
            let mb_query_ty = route.generate_query_type();
            tokens.extend(mb_query_ty)
        }
    }
    tokens
}

fn generate_rust_interface(
    routes: &Map<Vec<Route>>,
    title: &str,
    trait_name: &TypeName,
) -> TokenStream {
    let mut methods = TokenStream::new();
    let descr = doc_comment(format!("Api generated from '{}' spec", title));
    for (_, route_methods) in routes {
        for route in route_methods {
            methods.extend(route.generate_signature());
        }
    }
    quote! {
        #descr
        #[hsr::async_trait::async_trait(?Send)]
        pub trait #trait_name: 'static {
            type Error: HasStatusCode;
            fn new(host: Url) -> Self;

            #methods
        }
    }
}

fn generate_rust_dispatchers(routes: &Map<Vec<Route>>, trait_name: &TypeName) -> TokenStream {
    let mut dispatchers = TokenStream::new();
    for (_, route_methods) in routes {
        for route in route_methods {
            dispatchers.extend(route.generate_dispatcher(trait_name));
        }
    }
    quote! {#dispatchers}
}

fn generate_struct_def(
    name: &TypeName,
    descr: Option<&str>,
    Struct { fields }: &Struct,
    vis: Visibility,
) -> TokenStream {
    let name = ident(name);
    let fieldname = fields.iter().map(|f| &f.name);
    let fieldtype = fields.iter().map(|f| &f.ty);
    let fielddescr = fields
        .iter()
        .map(|f| f.ty.meta.description.as_ref().map(doc_comment));
    let descr = descr.as_ref().map(doc_comment);
    let derives = get_derive_tokens();
    let toks = quote! {
        #derives
        #descr
        #vis struct #name {
            #(
                #fielddescr
                pub #fieldname: #fieldtype
            ),*
        }
    };
    toks
}

// TODO this probably doesn't need to accept the whole API object
fn build_type(
    ref_or_schema: &ReferenceOr<Schema>,
    schema_lookup: &SchemaLookup,
) -> Result<StructOrType> {
    let schema = match ref_or_schema {
        ReferenceOr::Reference { reference } => {
            let (name, meta) = validate_schema_ref(reference, schema_lookup)?;
            return Ok(TypeInner::Named(name).with_meta_either(meta));
        }
        ReferenceOr::Item(item) => item,
    };
    let meta = schema.schema_data.clone();
    let ty = match &schema.schema_kind {
        SchemaKind::Type(ty) => ty,
        SchemaKind::Any(obj) => {
            if obj.properties.is_empty() {
                return Ok(TypeInner::Any.with_meta_either(meta));
            } else {
                return Struct::from_objlike(obj, schema_lookup).map(|s| s.with_meta_either(meta));
            }
        }
        SchemaKind::AllOf { all_of: schemas } => {
            let allof_types = schemas
                .iter()
                .map(|schema| build_type(schema, schema_lookup))
                .collect::<Result<Vec<_>>>()?;
            // It's an 'allOf'. We need to costruct a new type by combining other types together
            // return combine_types(&allof_types).map(|s| s.with_meta_either(meta))
            todo!()
        }
        _ => return Err(Error::UnsupportedKind(schema.schema_kind.clone())),
    };
    let typ = match ty {
        // TODO make enums from string
        // TODO fail on other validation
        ApiType::String(_) => TypeInner::String,
        ApiType::Number(_) => TypeInner::F64,
        ApiType::Integer(_) => TypeInner::I64,
        ApiType::Boolean {} => TypeInner::Bool,
        ApiType::Array(arr) => {
            let items = arr.items.clone().unbox();
            let inner = build_type(&items, schema_lookup).and_then(|t| t.discard_struct())?;
            TypeInner::Array(Box::new(inner))
        }
        ApiType::Object(obj) => {
            return Struct::from_objlike(obj, schema_lookup).map(|s| s.with_meta_either(meta))
        }
    };
    Ok(typ.with_meta_either(meta))
}

#[allow(dead_code)]
fn combine_types(
    types: &[StructOrType],
    lookup: &Map<ReferenceOr<StructOrType>>,
) -> Result<Struct> {
    let mut fields = IndexMap::new();
    for typ in types {
        let strukt = match &typ.typ {
            Either::Left(strukt) => strukt,
            // FIXME problem - we can have a Type::Named which we need to dereference :/
            Either::Right(_other) => {
                return Err(Error::Todo(
                    "Only object-like types allowed in AllOf types".to_string(),
                ))
            }
        };
        for field in &strukt.fields {
            if let Some(_) = fields.insert(&field.name, field) {
                return Err(Error::DuplicateName(field.name.to_string()));
            }
        }
    }
    let fields: Vec<_> = fields.values().cloned().cloned().collect();
    Struct::new(fields)
}

fn generate_rust_server(routemap: &Map<Vec<Route>>, trait_name: &TypeName) -> TokenStream {
    let resources: Vec<_> = routemap
        .iter()
        .map(|(path, routes)| {
            let (meth, opid): (Vec<_>, Vec<_>) = routes
                .iter()
                .map(|route| {
                    (
                        ident(route.method().to_string().to_snake_case()),
                        route.operation_id(),
                    )
                })
                .unzip();
            quote! {
                web::resource(#path)
                    #(.route(web::#meth().to(#opid::<A>)))*
            }
        })
        .collect();

    let server = quote! {
        #[allow(dead_code)]
        pub mod server {
            use super::*;

            fn configure_hsr<A: #trait_name + Send + Sync>(cfg: &mut actix_web::web::ServiceConfig) {
                cfg #(.service(#resources))*;
            }

            /// Serve the API on a given host.
            /// Once started, the server blocks indefinitely.
            pub async fn serve<A: #trait_name + Send + Sync>(cfg: hsr::Config) -> std::io::Result<()> {
                // We register the user-supplied Api as a Data item.
                // You might think it would be cleaner to generate out API trait
                // to not take "self" at all (only inherent impls) and then just
                // have Actix call those functions directly, like `.to(Api::func)`.
                // However we also want a way for the user to pass in arbitrary state to
                // handlers, so we kill two birds with one stone by stashing the Api
                // as data, pulling then it back out upon each request and calling
                // the handler as a method
                let api = AxData::new(A::new(cfg.host.clone()));

                let server = HttpServer::new(move || {
                    App::new()
                        .app_data(api.clone())
                        .wrap(Logger::default())
                        .configure(|cfg| hsr::configure_spec(cfg, JSON_SPEC, UI_TEMPLATE))
                        .configure(configure_hsr::<A>)
                });

                // Bind to socket
                let server = if let Some(ssl) = cfg.ssl {
                    server.bind_openssl((cfg.host.host_str().unwrap(), cfg.host.port().unwrap()), ssl)
                } else {
                    server.bind((cfg.host.host_str().unwrap(), cfg.host.port().unwrap()))
                }?;

                // run!
                server.run().await
            }
        }
    };
    server
}

fn generate_rust_client(routes: &Map<Vec<Route>>, trait_name: &TypeName) -> TokenStream {
    let mut method_impls = TokenStream::new();
    for (_, route_methods) in routes {
        for route in route_methods {
            method_impls.extend(route.generate_client_impl());
        }
    }

    quote! {
        #[allow(dead_code)]
        #[allow(unused_imports)]
        pub mod client {
            use super::*;
            use hsr::actix_http::http::Method;
            use hsr::awc::Client as ActixClient;
            use hsr::ClientError;
            use hsr::futures::future::{err as fut_err, ok as fut_ok};
            use hsr::serde_urlencoded;

            pub struct Client {
                domain: Url,
                inner: ActixClient,
            }

            #[hsr::async_trait::async_trait(?Send)]
            impl #trait_name for Client {
                type Error = ClientError;
                fn new(domain: Url) -> Self {
                    Client {
                        domain: domain,
                        inner: ActixClient::new()
                    }
                }

                #method_impls
            }
        }
    }
}

pub fn generate_from_yaml_file(yaml: impl AsRef<Path>) -> Result<String> {
    // TODO add generate_from_json_file
    let f = fs::File::open(yaml)?;
    generate_from_yaml_source(f)
}

pub fn generate_from_yaml_source(mut yaml: impl std::io::Read) -> Result<String> {
    let mut openapi_source = String::new();
    yaml.read_to_string(&mut openapi_source)?;
    let mut api: OpenAPI = serde_yaml::from_str(&openapi_source)?;
    let components = api.components.take().unwrap_or_default();
    let schema_lookup = components.schemas;
    let response_lookup = components.responses;
    let parameters_lookup = components.parameters;
    let req_body_lookup = components.request_bodies;

    let json_spec = serde_json::to_string(&api).expect("Bad api serialization");

    let trait_name = api_trait_name(&api);
    debug!("Gather types");
    let typs = gather_component_types(&schema_lookup)?;
    debug!("Gather routes");
    let routes = gather_routes(
        &api.paths,
        &schema_lookup,
        &response_lookup,
        &parameters_lookup,
        &req_body_lookup,
    )?;
    debug!("Generate component types");
    let rust_component_types = generate_rust_component_types(&typs);
    debug!("Generate route types");
    let rust_route_types = generate_rust_route_types(&routes);
    debug!("Generate API trait");
    let rust_trait = generate_rust_interface(&routes, &api.info.title, &trait_name);
    debug!("Generate dispatchers");
    let rust_dispatchers = generate_rust_dispatchers(&routes, &trait_name);
    debug!("Generate server");
    let rust_server = generate_rust_server(&routes, &trait_name);
    debug!("Generate clientr");
    let rust_client = generate_rust_client(&routes, &trait_name);
    let code = quote! {
        #[allow(dead_code)]

        // Dump the spec and the ui template in the source file, for serving ui
        const JSON_SPEC: &'static str = #json_spec;
        const UI_TEMPLATE: &'static str = #SWAGGER_UI_TEMPLATE;

        mod __imports {
            pub use hsr::{HasStatusCode, Void};
            pub use hsr::actix_web::{
                self, App, HttpServer, HttpRequest, HttpResponse, Responder, Either as AxEither,
                web::{self, Json as AxJson, Query as AxQuery, Path as AxPath, Data as AxData, ServiceConfig},
                middleware::Logger
            };
            pub use hsr::url::Url;
            pub use hsr::actix_http::http::{StatusCode};
            pub use hsr::futures::future::{Future, FutureExt, TryFutureExt, Ready, ok as fut_ok};

            // macros re-exported from `serde-derive`
            pub use hsr::{Serialize, Deserialize};
        }
        #[allow(dead_code)]
        use __imports::*;

        // TypeInner definitions
        #rust_component_types
        #rust_route_types
        // Interface definition
        #rust_trait
        // Dispatcher definitions
        #rust_dispatchers
        // Server
        #rust_server
        // Client
        #rust_client
    };
    let code = code.to_string();
    #[cfg(feature = "rustfmt")]
    {
        debug!("Prettify");
        prettify_code(code)
    }
    #[cfg(not(feature = "rustfmt"))]
    {
        Ok(code)
    }
}

/// Run the code through `rustfmt`.
#[cfg(feature = "rustfmt")]
pub fn prettify_code(input: String) -> Result<String> {
    let mut buf = Vec::new();
    {
        let mut config = rustfmt_nightly::Config::default();
        config.set().emit_mode(rustfmt_nightly::EmitMode::Stdout);
        config.set().edition(rustfmt_nightly::Edition::Edition2018);
        let mut session = rustfmt_nightly::Session::new(config, Some(&mut buf));
        session
            .format(rustfmt_nightly::Input::Text(input))
            .map_err(|_e| Error::BadCodegen)?;
    }
    if buf.is_empty() {
        return Err(Error::BadCodegen);
    }
    let mut s = String::from_utf8(buf).unwrap();
    // TODO no idea why this is necessary but... it is
    if s.starts_with("stdin:\n\n") {
        s = s.split_off(8);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snake_casify() {
        assert_eq!("/a/b/c".to_snake_case(), "a_b_c");
        assert_eq!(
            "/All/ThisIs/justFine".to_snake_case(),
            "all_this_is_just_fine"
        );
        assert_eq!("/{someId}".to_snake_case(), "some_id");
        assert_eq!(
            "/123_abc{xyz\\!\"£$%^}/456 asdf".to_snake_case(),
            "123_abc_xyz_456_asdf"
        )
    }

    #[test]
    fn test_analyse_path() {
        use PathSegment::*;

        // Should fail
        assert!(analyse_path("").is_err());
        assert!(analyse_path("a/b").is_err());
        assert!(analyse_path("/a/b/c/").is_err());
        assert!(analyse_path("/a{").is_err());
        assert!(analyse_path("/a{}").is_err());
        assert!(analyse_path("/{}a").is_err());
        assert!(analyse_path("/{a}a").is_err());
        assert!(analyse_path("/ a").is_err());

        // TODO probably should succeed
        assert!(analyse_path("/a1").is_err());
        assert!(analyse_path("/{a1}").is_err());

        // Should succeed
        assert_eq!(
            analyse_path("/a/b").unwrap(),
            vec![Literal("a".into()), Literal("b".into()),]
        );
        assert_eq!(
            analyse_path("/{test}").unwrap(),
            vec![Parameter("test".into())]
        );
        assert_eq!(
            analyse_path("/{a}/{b}/a/b").unwrap(),
            vec![
                Parameter("a".into()),
                Parameter("b".into()),
                Literal("a".into()),
                Literal("b".into())
            ]
        );
    }

    // #[test]
    // fn test_build_types_complex() {
    //     let yaml = "example-api/petstore-expanded.yaml";
    //     let yaml = fs::read_to_string(yaml).unwrap();
    //     let api: OpenAPI = serde_yaml::from_str(&yaml).unwrap();
    //     gather_types(&api).unwrap();
    // }
}
