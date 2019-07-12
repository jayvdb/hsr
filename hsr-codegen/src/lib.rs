use std::fmt;
use std::fs;
use std::path::Path;

use derive_more::{Display, From};
use failure::Fail;
use heck::{CamelCase, SnakeCase};
use log::{debug, info};
use openapiv3::{OpenAPI, ReferenceOr, Schema, SchemaKind, Type as ApiType};
use proc_macro2::{Ident as QIdent, TokenStream};
use quote::quote;
use regex::Regex;

fn ident(s: impl fmt::Display) -> QIdent {
    QIdent::new(&s.to_string(), proc_macro2::Span::call_site())
}

pub type Map<T> = std::collections::BTreeMap<String, T>;
pub type TypeMap<T> = std::collections::BTreeMap<TypeName, T>;
pub type IdMap<T> = std::collections::BTreeMap<Ident, T>;

#[derive(Debug, From, Fail)]
pub enum Error {
    #[fail(display = "IO Error: {}", _0)]
    Io(std::io::Error),
    #[fail(display = "Yaml Error: {}", _0)]
    Yaml(serde_yaml::Error),
    #[fail(display = "Codegen failed")]
    CodeGen,
    #[fail(display = "Bad reference: \"{}\"", _0)]
    BadReference(String),
    #[fail(display = "Unexpected reference: \"{}\"", _0)]
    UnexpectedReference(String),
    #[fail(display = "Schema not supported: {:?}", _0)]
    UnsupportedKind(SchemaKind),
    #[fail(display = "Definition is too complex: {:?}", _0)]
    TooComplex(Schema),
    #[fail(display = "Empty struct")]
    EmptyStruct,
    #[fail(display = "Rust does not support structural typing")]
    NotStructurallyTyped,
    #[fail(display = "Path is malformed: {}", _0)]
    MalformedPath(String),
    #[fail(display = "No operation id given for route {}", _0)]
    NoOperationId(String),
    #[fail(display = "TODO: {}", _0)]
    Todo(String),
    #[fail(display = "{} is not a valid identifier", _0)]
    BadIdentifier(String),
    #[fail(display = "{} is not a valid type name", _0)]
    BadTypeName(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn unwrap_ref<T>(item: &ReferenceOr<T>) -> Result<&T> {
    match item {
        ReferenceOr::Item(item) => Ok(item),
        ReferenceOr::Reference { reference } => {
            Err(Error::UnexpectedReference(reference.to_string()))
        }
    }
}

fn dereference<'a, T>(
    refr: &'a ReferenceOr<T>,
    lookup: Option<&'a Map<ReferenceOr<T>>>,
) -> Result<&'a T> {
    match refr {
        ReferenceOr::Reference { reference } => unimplemented!(),
        ReferenceOr::Item(item) => Ok(item),
    }
}

fn validate_ref_id<'a>(refr: &'a str, api: &'a OpenAPI) -> Result<TypeName> {
    let name = extract_ref_name(refr)?;
    // Do the lookup. We are just checking the ref points to 'something'
    // TODO look out for circular ref
    let _ = api
        .components
        .as_ref()
        .and_then(|c| c.schemas.get(&name.to_string()))
        .ok_or(Error::BadReference(refr.to_string()))?;
    Ok(name)
}

fn extract_ref_name(refr: &str) -> Result<TypeName> {
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

fn gather_types(api: &OpenAPI) -> Result<TypeMap<Type>> {
    let mut typs = TypeMap::new();
    // gather types defined in components
    if let Some(component) = &api.components {
        for (name, schema) in &component.schemas {
            info!("Processing schema: {}", name);
            let typename = TypeName::new(name.clone())?;
            let typ = build_type(&schema, api)?;
            assert!(typs.insert(typename, typ).is_none());
        }
    }
    Ok(typs)
}

#[derive(Debug, Clone)]
enum Method {
    Get,
    Post(Type),
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Method::Get => write!(f, "get"),
            Method::Post(_) => write!(f, "post"),
        }
    }
}

// Route contains all the information necessary to contruct the API
// If it has been constructed, the route is logically sound
#[derive(Debug, Clone)]
struct Route {
    operation_id: Ident,
    method: Method,
    path_args: Vec<(Ident, Type)>,
    query_args: Vec<(Ident, Type)>,
    segments: Vec<PathSegment>,
}

impl Route {
    fn generate_interface(&self) -> Result<TokenStream> {
        let opid = ident(&self.operation_id);
        Ok(quote! {
            fn #opid(&self, test: Test) -> BoxFuture<Test>;
        })
    }
}

/// A string which is a valid identifier (snake_case)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display)]
struct Ident(String);

impl Ident {
    fn new(val: String) -> Result<Ident> {
        if val == val.to_snake_case() {
            Ok(Ident(val))
        } else {
            Err(Error::BadIdentifier(val))
        }
    }
}

/// A string which is a valid name for type (CamelCase)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSegment {
    Literal(String),
    Parameter(String),
}

fn analyse_path(path: &str) -> Result<Vec<PathSegment>> {
    // TODO lazy static
    let literal_re = Regex::new("^[[:alpha:]]+$").unwrap();
    let param_re = Regex::new(r#"^\{([[:alpha:]]+)\}$"#).unwrap();

    if path.is_empty() || !path.starts_with('/') {
        return Err(Error::MalformedPath(path.to_string()));
    }

    let mut segments = Vec::new();

    for segment in path.split('/').skip(1) {
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
    Ok(segments)
}

fn gather_routes(api: &OpenAPI) -> Result<Map<Vec<Route>>> {
    let mut routes = Map::new();
    println!("{:?}", api.paths.keys());
    for (path, pathitem) in &api.paths {
        debug!("Processing path: {:?}", path);
        let pathitem = unwrap_ref(&pathitem)?;
        let segments = analyse_path(path)?;
        let mut pathroutes = Vec::new();
        if let Some(ref op) = pathitem.get {
            let method = Method::Get;
            let operation_id = match op.operation_id {
                Some(ref op) => Ident::new(op.to_snake_case())?,
                None => return Err(Error::NoOperationId(path.clone())),
            };
            let route = Route {
                operation_id,
                path_args: vec![],
                query_args: vec![],
                segments: segments.clone(),
                method,
            };
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }
        if let Some(ref op) = pathitem.post {
            let operation_id = match op.operation_id {
                Some(ref op) => Ident::new(op.to_snake_case())?,
                None => return Err(Error::NoOperationId(path.clone())),
            };
            let body = op
                .request_body
                .as_ref()
                .ok_or_else(|| Error::Todo("'post' request has no body".into()))
                .and_then(|body| {
                    dereference(body, api.components.as_ref().map(|c| &c.request_bodies))
                })?;
            if !(body.content.len() == 1 && body.content.contains_key("application/json")) {
                return Err(Error::Todo(
                    "Request body must by application/json only".into(),
                ));
            }
            let ref_or_schema = body
                .content
                .get("application/json")
                .unwrap()
                .schema
                .as_ref()
                .ok_or_else(|| Error::Todo("Media type does not contain schema".into()))?;
            let method = Method::Post(build_type(&ref_or_schema, api)?);
            let route = Route {
                operation_id,
                path_args: vec![],
                query_args: vec![],
                segments: segments.clone(),
                method,
            };
            debug!("Add route: {:?}", route);
            pathroutes.push(route)
        }
        assert!(routes.insert(path.to_string(), pathroutes).is_none());
    }
    Ok(routes)
}

fn generate_rust_types(typs: &TypeMap<Type>) -> Result<TokenStream> {
    let mut tokens = TokenStream::new();
    for (typename, typ) in typs {
        let def = if let Type::Struct(fields) = typ {
            define_struct(typename, fields)?
        } else {
            let name = ident(typename);
            let typ = typ.to_token()?;
            quote! {
                type #name = #typ;
            }
        };
        tokens.extend(def);
    }
    Ok(tokens)
}

fn generate_rust_interface(routes: &Map<Vec<Route>>) -> Result<TokenStream> {
    let mut methods = TokenStream::new();
    for (_, route_methods) in routes {
        for route in route_methods {
            methods.extend(route.generate_interface()?);
        }
    }
    Ok(quote! {
        pub trait Api: Send + Sync + 'static {
            fn new() -> Self;

            #methods
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Type {
    String,
    F64,
    I64,
    Bool,
    Array(Box<Type>),
    Struct(Map<Type>),
    Any,
    Named(TypeName),
}

fn define_struct(name: &TypeName, elems: &Map<Type>) -> Result<TokenStream> {
    if elems.is_empty() {
        return Err(Error::EmptyStruct);
    }
    let name = ident(name);
    let field = elems.keys().map(|s| ident(s));
    let fieldtype = elems
        .values()
        .map(|s| s.to_token())
        .collect::<Result<Vec<_>>>()?;
    let toks = quote! {
        struct #name {
            #(#field: #fieldtype),*
        }
    };
    Ok(toks)
}

impl Type {
    fn is_complex(&self) -> bool {
        match self {
            Type::Array(typ) => match &**typ {
                // Vec<i32>, Vec<Vec<i32>> are simple, Vec<MyStruct> is complex
                Type::Array(inner) => inner.is_complex(),
                Type::Struct(_) => true,
                _ => false,
            },
            Type::Struct(_) => true,
            _ => false,
        }
    }

    fn to_token(&self) -> Result<TokenStream> {
        use Type::*;
        let s = match self {
            String => quote! { String },
            F64 => quote! { f64 },
            I64 => quote! { i64 },
            Bool => quote! { bool },
            Array(elem) => {
                let inner = elem.to_token()?;
                quote! { Vec<#inner> }
            }
            Named(name) => {
                let name = ident(name);
                quote! { #name }
            }
            // TODO handle Any properly
            Any => quote! { Any },
            Struct(_) => return Err(Error::NotStructurallyTyped),
        };
        Ok(s)
    }
}

macro_rules! typ_from_objlike {
    ($obj: ident, $api: ident) => {{
        let mut fields = Map::new();
        for (name, schemaref) in &$obj.properties {
            let schemaref = schemaref.clone().unbox();
            let inner = build_type(&schemaref, $api)?;
            assert!(fields.insert(name.clone(), inner).is_none());
        }
        Ok(Type::Struct(fields))
    }};
}

fn build_type(ref_or_schema: &ReferenceOr<Schema>, api: &OpenAPI) -> Result<Type> {
    let schema = match ref_or_schema {
        ReferenceOr::Reference { reference } => {
            let name = validate_ref_id(reference, api)?;
            return Ok(Type::Named(name));
        }
        ReferenceOr::Item(item) => item,
    };
    let ty = match &schema.schema_kind {
        SchemaKind::Type(ty) => ty,
        SchemaKind::Any(obj) => {
            if obj.properties.is_empty() {
                return Ok(Type::Any);
            } else {
                return typ_from_objlike!(obj, api);
            }
        }
        _ => return Err(Error::UnsupportedKind(schema.schema_kind.clone())),
    };
    let typ = match ty {
        // TODO make enums from string
        // TODO fail on other validation
        ApiType::String(_) => Type::String,
        ApiType::Number(_) => Type::F64,
        ApiType::Integer(_) => Type::I64,
        ApiType::Boolean {} => Type::Bool,
        ApiType::Array(arr) => {
            let items = arr.items.clone().unbox();
            let inner = build_type(&items, api)?;
            Type::Array(Box::new(inner))
        }
        ApiType::Object(obj) => {
            return typ_from_objlike!(obj, api);
        }
    };
    Ok(typ)
}

pub fn generate_from_yaml(yaml: impl AsRef<Path>) -> Result<String> {
    let f = fs::File::open(yaml)?;
    generate_from_yaml_source(f)
}

pub fn generate_from_yaml_source(yaml: impl std::io::Read) -> Result<String> {
    let api: OpenAPI = serde_yaml::from_reader(yaml)?;
    let typs = gather_types(&api)?;
    let routes = gather_routes(&api)?;
    let rust_defs = generate_rust_types(&typs)?;
    let rust_trait = generate_rust_interface(&routes)?;
    let code = quote! {
        use hsr_runtime;

        // TODO remove
        pub struct Test;
        // Type definitions
        #rust_defs
        // Interface definition
        #rust_trait
    };
    Ok(prettify_code(code.to_string()))
}

fn prettify_code(input: String) -> String {
    let mut buf = Vec::new();
    {
        let mut config = rustfmt_nightly::Config::default();
        config.set().emit_mode(rustfmt_nightly::EmitMode::Stdout);
        let mut session = rustfmt_nightly::Session::new(config, Some(&mut buf));
        session.format(rustfmt_nightly::Input::Text(input)).unwrap();
    }
    String::from_utf8(buf).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_diff(left: &str, right: &str) {
        use diff::Result::*;
        if left.contains(&right) {
            return;
        }
        for d in diff::lines(left, right) {
            match d {
                Left(l) => println!("- {}", l),
                Right(r) => println!("+ {}", r),
                Both(l, _) => println!("= {}", l),
            }
        }
        panic!("Bad diff")
    }

    #[test]
    fn test_build_types_simple() {
        let yaml = "../example-api/petstore.yaml";
        let code = generate_from_yaml(yaml).unwrap();
        let expect = quote! {
            pub trait Api: Send + Sync + 'static {
                fn new() -> Self;
                fn get_all_pets(&self) -> BoxFuture<Result<Vec<Pet>, Error>>;
                fn get_pet(&self, pet_id: u32) -> BoxFuture<Result<Pet, Error>>;
                fn create_pet(&self, pet: NewPet) -> BoxFuture<Result<Pet, Error>>;
            }
        }
        .to_string();
        let expect = prettify_code(expect);
        assert_diff(&code, &expect);
    }

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
