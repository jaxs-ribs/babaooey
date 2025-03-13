use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use syn::{self, Attribute, ImplItem, Item, Type};
use walkdir::WalkDir;
use toml::Value;

// Helper functions for naming conventions
fn to_kebab_case(s: &str) -> String {
    // First, handle the case where the input has underscores
    if s.contains('_') {
        return s.replace('_', "-");
    }
    
    let mut result = String::with_capacity(s.len() + 5); // Extra capacity for hyphens
    let chars: Vec<char> = s.chars().collect();
    
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            // Add hyphen if:
            // 1. Not the first character
            // 2. Previous character is lowercase
            // 3. Or next character is lowercase (to handle acronyms like HTML)
            if i > 0 && 
               (chars[i-1].is_lowercase() ||
                (i < chars.len() - 1 && chars[i+1].is_lowercase()))
            {
                result.push('-');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    
    result
}

// Validates a name doesn't contain numbers or "stream"
fn validate_name(name: &str, kind: &str) -> Result<()> {
    // Check for numbers
    if name.chars().any(|c| c.is_digit(10)) {
        anyhow::bail!("Error: {} name '{}' contains numbers, which is not allowed", kind, name);
    }
    
    // Check for "stream"
    if name.to_lowercase().contains("stream") {
        anyhow::bail!("Error: {} name '{}' contains 'stream', which is not allowed", kind, name);
    }
    
    Ok(())
}

// Remove "State" suffix from a name
fn remove_state_suffix(name: &str) -> String {
    if name.ends_with("State") {
        let len = name.len();
        return name[0..len-5].to_string();
    }
    name.to_string()
}

// Extract wit_world from the #[hyperprocess] attribute using the format in the debug representation
fn extract_wit_world(attrs: &[Attribute]) -> Result<String> {
    for attr in attrs {
        if attr.path().is_ident("hyperprocess") {
            // Convert attribute to string representation
            let attr_str = format!("{:?}", attr);
            println!("Attribute string: {}", attr_str);
            
            // Look for wit_world in the attribute string
            if let Some(pos) = attr_str.find("wit_world") {
                println!("Found wit_world at position {}", pos);
                
                // Find the literal value after wit_world by looking for lit: "value"
                let lit_pattern = "lit: \"";
                if let Some(lit_pos) = attr_str[pos..].find(lit_pattern) {
                    let start_pos = pos + lit_pos + lit_pattern.len();
                    
                    // Find the closing quote of the literal
                    if let Some(quote_pos) = attr_str[start_pos..].find('\"') {
                        let world_name = &attr_str[start_pos..(start_pos + quote_pos)];
                        println!("Extracted wit_world: {}", world_name);
                        return Ok(world_name.to_string());
                    }
                }
            }
        }
    }
    anyhow::bail!("wit_world not found in hyperprocess attribute")
}

// Convert Rust type to WIT type, including downstream types
fn rust_type_to_wit(ty: &Type, used_types: &mut HashSet<String>) -> Result<String> {
    match ty {
        Type::Path(type_path) => {
            if type_path.path.segments.is_empty() {
                return Ok("unknown".to_string());
            }
            
            let ident = &type_path.path.segments.last().unwrap().ident;
            let type_name = ident.to_string();
            
            match type_name.as_str() {
                "i32" => Ok("s32".to_string()),
                "u32" => Ok("u32".to_string()),
                "i64" => Ok("s64".to_string()),
                "u64" => Ok("u64".to_string()),
                "f32" => Ok("f32".to_string()),
                "f64" => Ok("f64".to_string()),
                "String" => Ok("string".to_string()),
                "bool" => Ok("bool".to_string()),
                "Vec" => {
                    if let syn::PathArguments::AngleBracketed(args) = 
                        &type_path.path.segments.last().unwrap().arguments
                    {
                        if let Some(syn::GenericArgument::Type(inner_ty)) = args.args.first() {
                            let inner_type = rust_type_to_wit(inner_ty, used_types)?;
                            Ok(format!("list<{}>", inner_type))
                        } else {
                            Ok("list<any>".to_string())
                        }
                    } else {
                        Ok("list<any>".to_string())
                    }
                }
                "Option" => {
                    if let syn::PathArguments::AngleBracketed(args) =
                        &type_path.path.segments.last().unwrap().arguments
                    {
                        if let Some(syn::GenericArgument::Type(inner_ty)) = args.args.first() {
                            let inner_type = rust_type_to_wit(inner_ty, used_types)?;
                            Ok(format!("option<{}>", inner_type))
                        } else {
                            Ok("option<any>".to_string())
                        }
                    } else {
                        Ok("option<any>".to_string())
                    }
                }
                custom => {
                    // Validate custom type name
                    validate_name(custom, "Type")?;
                    
                    // Convert custom type to kebab-case and add to used types
                    let kebab_custom = to_kebab_case(custom);
                    used_types.insert(kebab_custom.clone());
                    Ok(kebab_custom)
                }
            }
        }
        Type::Reference(type_ref) => {
            // Handle references by using the underlying type
            rust_type_to_wit(&type_ref.elem, used_types)
        }
        Type::Tuple(type_tuple) => {
            if type_tuple.elems.is_empty() {
                // Empty tuple is unit in WIT
                Ok("unit".to_string())
            } else {
                // Create a tuple representation in WIT
                let mut elem_types = Vec::new();
                for elem in &type_tuple.elems {
                    elem_types.push(rust_type_to_wit(elem, used_types)?);
                }
                Ok(format!("tuple<{}>", elem_types.join(", ")))
            }
        }
        _ => Ok("unknown".to_string()),
    }
}

// Collect type definitions (structs and enums) from the file
fn collect_type_definitions(ast: &syn::File) -> Result<HashMap<String, String>> {
    let mut type_defs = HashMap::new();
    
    println!("Collecting type definitions from file");
    for item in &ast.items {
        match item {
            Item::Struct(item_struct) => {
                // Validate struct name doesn't contain numbers or "stream"
                let orig_name = item_struct.ident.to_string();
                validate_name(&orig_name, "Struct")?;
                
                // Use kebab-case for struct name
                let name = to_kebab_case(&orig_name);
                println!("  Found struct: {}", name);
                
                let fields: Vec<String> = match &item_struct.fields {
                    syn::Fields::Named(fields) => {
                        let mut used_types = HashSet::new();
                        let mut field_strings = Vec::new();
                        
                        for f in &fields.named {
                            if let Some(field_ident) = &f.ident {
                                // Validate field name doesn't contain digits
                                let field_orig_name = field_ident.to_string();
                                validate_name(&field_orig_name, "Field")?;
                                
                                // Convert field names to kebab-case
                                let field_name = to_kebab_case(&field_orig_name);
                                let field_type = rust_type_to_wit(&f.ty, &mut used_types)?;
                                println!("    Field: {} -> {}", field_name, field_type);
                                field_strings.push(format!("        {}: {}", field_name, field_type));
                            }
                        }
                        
                        field_strings
                    }
                    _ => Vec::new(),
                };
                
                if !fields.is_empty() {
                    type_defs.insert(
                        name.clone(),
                        format!("    record {} {{\n{}\n    }}", name, fields.join(",\n")), // Add comma separator
                    );
                }
            }
            Item::Enum(item_enum) => {
                // Validate enum name doesn't contain numbers or "stream"
                let orig_name = item_enum.ident.to_string();
                validate_name(&orig_name, "Enum")?;
                
                // Use kebab-case for enum name
                let name = to_kebab_case(&orig_name);
                println!("  Found enum: {}", name);
                
                let variants: Vec<String> = item_enum
                    .variants
                    .iter()
                    .map(|v| {
                        let variant_orig_name = v.ident.to_string();
                        // Validate variant name
                        validate_name(&variant_orig_name, "Enum variant")?;
                        
                        match &v.fields {
                            syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                                let mut used_types = HashSet::new();
                                let ty = rust_type_to_wit(
                                    &fields.unnamed.first().unwrap().ty,
                                    &mut used_types
                                )?;
                                
                                // Use kebab-case for variant names and use parentheses for type
                                let variant_name = to_kebab_case(&variant_orig_name);
                                println!("    Variant: {} -> {}", variant_name, ty);
                                Ok(format!("        {}({})", variant_name, ty))
                            }
                            syn::Fields::Unit => {
                                // Use kebab-case for variant names
                                let variant_name = to_kebab_case(&variant_orig_name);
                                println!("    Variant: {}", variant_name);
                                Ok(format!("        {}", variant_name))
                            },
                            _ => {
                                // Use kebab-case for variant names
                                let variant_name = to_kebab_case(&variant_orig_name);
                                println!("    Variant: {} (complex)", variant_name);
                                Ok(format!("        {}", variant_name))
                            },
                        }
                    })
                    .collect::<Result<Vec<String>>>()?;
                
                type_defs.insert(
                    name.clone(),
                    format!("    variant {} {{\n{}\n    }}", name, variants.join(",\n")), // Add comma separator
                );
            }
            _ => {}
        }
    }
    
    println!("Collected {} type definitions", type_defs.len());
    Ok(type_defs)
}

// Generate WIT content for an interface
fn generate_interface_wit_content(
    impl_item: &syn::ItemImpl,
    interface_name: &str,
    ast: &syn::File,
) -> Result<String> {
    let mut functions = Vec::new();
    let mut used_types = HashSet::new();
    
    // Extract the base name without "State" suffix for the interface
    let base_name = remove_state_suffix(interface_name);
    
    // Convert interface name to kebab-case for the interface declaration
    let kebab_interface_name = to_kebab_case(&base_name);
    println!("Generating WIT content for interface: {} (kebab: {})", interface_name, kebab_interface_name);
    
    for item in &impl_item.items {
        if let ImplItem::Fn(method) = item {
            let method_name = method.sig.ident.to_string();
            println!("  Examining method: {}", method_name);
            
            let has_remote = method.attrs.iter().any(|attr| attr.path().is_ident("remote"));
            let has_local = method.attrs.iter().any(|attr| attr.path().is_ident("local"));
            let has_http = method.attrs.iter().any(|attr| attr.path().is_ident("http"));
            
            let has_relevant_attr = has_remote || has_local || has_http;
            
            if has_relevant_attr {
                println!("    Has relevant attribute: {}", 
                    if has_remote { "remote" } 
                    else if has_local { "local" } 
                    else { "http" });
                
                let sig = &method.sig;
                
                // Validate function name
                validate_name(&method_name, "Function")?;
                
                // Convert function name to kebab-case
                let kebab_name = to_kebab_case(&method_name);
                println!("    Processing method: {} -> {}", method_name, kebab_name);
                
                let params: Vec<String> = sig
                    .inputs
                    .iter()
                    .filter_map(|arg| {
                        if let syn::FnArg::Typed(pat_type) = arg {
                            if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                                // Skip &self and &mut self
                                if pat_ident.ident == "self" {
                                    println!("      Skipping self parameter");
                                    return None;
                                }
                                
                                // Get original param name and convert to kebab-case
                                let param_orig_name = pat_ident.ident.to_string();
                                
                                // Validate parameter name
                                match validate_name(&param_orig_name, "Parameter") {
                                    Ok(_) => {},
                                    Err(e) => return Some(Err(e)),
                                }
                                
                                let param_name = to_kebab_case(&param_orig_name);
                                
                                // Rust type to WIT type
                                match rust_type_to_wit(&pat_type.ty, &mut used_types) {
                                    Ok(param_type) => {
                                        println!("      Parameter: {} -> {}", param_name, param_type);
                                        Some(Ok(format!("{}: {}", param_name, param_type)))
                                    },
                                    Err(e) => Some(Err(e))
                                }
                            } else {
                                println!("      Skipping non-ident pattern");
                                None
                            }
                        } else {
                            println!("      Skipping non-typed argument");
                            None
                        }
                    })
                    .collect::<Result<Vec<String>>>()?;
                
                let return_type = match &sig.output {
                    syn::ReturnType::Type(_, ty) => {
                        let rt = rust_type_to_wit(&*ty, &mut used_types)?;
                        println!("      Return type: {} -> result<{}, string>", rt, rt);
                        format!("result<{}, string>", rt)
                    }
                    _ => {
                        println!("      Return type: unit -> result<unit, string>");
                        "result<unit, string>".to_string()
                    }
                };
                
                // Generate attribute comments with proper indentation
                let mut attr_comments = Vec::new();
                if has_remote {
                    attr_comments.push("    //remote");
                }
                if has_local {
                    attr_comments.push("    //local");
                }
                if has_http {
                    attr_comments.push("    //http");
                }
                let attr_comment_str = if !attr_comments.is_empty() {
                    format!("{}\n", attr_comments.join("\n"))
                } else {
                    String::new()
                };
                
                let func_sig = if params.is_empty() {
                    format!("{}    {}: func(target: address) -> {};", 
                        attr_comment_str,
                        kebab_name, 
                        return_type) 
                } else {
                    format!("{}    {}: func(target: address, {}) -> {};",
                        attr_comment_str,
                        kebab_name,
                        params.join(", "), // Use comma separator
                        return_type
                    ) 
                };
                
                println!("    Added function: {}", func_sig);
                functions.push(func_sig);
            } else {
                println!("    Skipping method without relevant attributes");
            }
        }
    }
    
    // Collect all type definitions from the file
    let all_type_defs = collect_type_definitions(ast)?;
    
    // Filter for only the types we're using
    let mut type_defs = Vec::new();
    let mut processed_types = HashSet::new();
    let mut types_to_process: Vec<String> = used_types.into_iter().collect();
    
    println!("Processing used types: {:?}", types_to_process);
    
    // Process all referenced types and their dependencies
    while let Some(type_name) = types_to_process.pop() {
        if processed_types.contains(&type_name) {
            continue;
        }
        
        processed_types.insert(type_name.clone());
        println!("  Processing type: {}", type_name);
        
        if let Some(type_def) = all_type_defs.get(&type_name) {
            println!("    Found type definition");
            type_defs.push(type_def.clone());
            
            // Extract any types referenced in this type definition
            for referenced_type in all_type_defs.keys() {
                if type_def.contains(referenced_type) && !processed_types.contains(referenced_type) {
                    println!("    Adding referenced type: {}", referenced_type);
                    types_to_process.push(referenced_type.clone());
                }
            }
        } else {
            println!("    No definition found for type: {}", type_name);
        }
    }
    
    // Generate the final WIT content
    if functions.is_empty() {
        println!("No functions found for interface {}", interface_name);
        Ok(String::new())
    } else {
        // Combine type definitions and functions within the interface block
        let combined_content = if type_defs.is_empty() {
            format!("    use standard.{{address}};\n\n{}", functions.join("\n"))
        } else {
            format!("    use standard.{{address}};\n\n{}\n\n{}", type_defs.join("\n\n"), functions.join("\n"))
        };
        
        let content = format!("interface {} {{\n{}\n}}\n", kebab_interface_name, combined_content);
        println!("Generated interface content for {} with {} type definitions", interface_name, type_defs.len());
        Ok(content)
    }
}

// Process a single Rust project and generate WIT files
fn process_rust_project(project_path: &Path, api_dir: &Path) -> Result<Option<String>> {
    println!("\nProcessing project: {}", project_path.display());
    let lib_rs = project_path.join("src").join("lib.rs");
    
    println!("Looking for lib.rs at {}", lib_rs.display());
    if !lib_rs.exists() {
        println!("No lib.rs found for project: {}", project_path.display());
        return Ok(None);
    }
    
    let lib_content = fs::read_to_string(&lib_rs)
        .with_context(|| format!("Failed to read lib.rs for project: {}", project_path.display()))?;
    
    println!("Successfully read lib.rs, parsing...");
    let ast = syn::parse_file(&lib_content)
        .with_context(|| format!("Failed to parse lib.rs for project: {}", project_path.display()))?;
    
    println!("Successfully parsed lib.rs");
    
    let mut wit_world = None;
    let mut interface_name = None;
    let mut kebab_interface_name = None;
    
    println!("Scanning for impl blocks with hyperprocess attribute");
    for item in &ast.items {
        if let Item::Impl(impl_item) = item {
            println!("Found impl block");
            
            // Check if this impl block has a #[hyperprocess] attribute
            if let Some(attr) = impl_item.attrs.iter().find(|attr| attr.path().is_ident("hyperprocess")) {
                println!("Found hyperprocess attribute");
                
                // Extract the wit_world name
                match extract_wit_world(&[attr.clone()]) {
                    Ok(world_name) => {
                        println!("Extracted wit_world: {}", world_name);
                        wit_world = Some(world_name);
                        
                        // Get the interface name from the impl type
                        interface_name = impl_item
                            .self_ty
                            .as_ref()
                            .as_type_path()
                            .map(|tp| {
                                if let Some(last_segment) = tp.path.segments.last() {
                                    last_segment.ident.to_string()
                                } else {
                                    "Unknown".to_string()
                                }
                            });
                        
                        // Check for "State" suffix and remove it
                        if let Some(ref name) = interface_name {
                            // Validate the interface name
                            validate_name(name, "Interface")?;
                            
                            // Remove State suffix if present
                            let base_name = remove_state_suffix(name);
                            
                            // Convert to kebab-case for file name and interface name
                            kebab_interface_name = Some(to_kebab_case(&base_name));
                            
                            println!("Interface name: {:?}", interface_name);
                            println!("Base name: {}", base_name);
                            println!("Kebab interface name: {:?}", kebab_interface_name);
                        }
                        
                        if let (Some(ref iface_name), Some(ref kebab_name)) = (&interface_name, &kebab_interface_name) {
                            // We already validated the interface name, so the file name should be fine
                            
                            // Generate the WIT content
                            let content = generate_interface_wit_content(impl_item, iface_name, &ast)?;
                            
                            if !content.is_empty() {
                                // Write the interface file with kebab-case name
                                let interface_file = api_dir.join(format!("{}.wit", kebab_name));
                                println!("Writing WIT file to {}", interface_file.display());
                                
                                fs::write(&interface_file, &content)
                                    .with_context(|| format!("Failed to write {}", interface_file.display()))?;
                                
                                println!("Successfully wrote WIT file");
                            } else {
                                println!("Generated WIT content is empty, skipping file creation");
                            }
                        }
                    },
                    Err(e) => println!("Failed to extract wit_world: {}", e),
                }
            }
        }
    }
    
    if let (Some(_), Some(_), Some(kebab_iface)) = (wit_world, interface_name, kebab_interface_name) {
        println!("Returning export statement for interface {}", kebab_iface);
        // Use kebab-case interface name for export (changed from import to export)
        Ok(Some(format!("    export {};", kebab_iface)))
    } else {
        println!("No valid interface found");
        Ok(None)
    }
}

// Helper trait to get TypePath from Type
trait AsTypePath {
    fn as_type_path(&self) -> Option<&syn::TypePath>;
}

impl AsTypePath for syn::Type {
    fn as_type_path(&self) -> Option<&syn::TypePath> {
        match self {
            syn::Type::Path(tp) => Some(tp),
            _ => None,
        }
    }
}

fn main() -> Result<()> {
    // Get the current working directory
    let cwd = std::env::current_dir()?;
    println!("Current working directory: {}", cwd.display());
    
    // Create the api directory if it doesn't exist
    let api_dir = cwd.join("api");
    println!("API directory: {}", api_dir.display());
    
    fs::create_dir_all(&api_dir)?;
    println!("Created or verified api directory");
    
    // Find all relevant Rust projects
    let projects = find_rust_projects(&cwd);
    
    if projects.is_empty() {
        println!("No relevant Rust projects found.");
        return Ok(());
    }
    
    println!("Found {} relevant Rust projects.", projects.len());
    
    // Process each project and collect world exports
    let mut world_exports = Vec::new();
    let mut world_names = HashSet::new();
    
    for project_path in projects {
        println!("Processing project: {}", project_path.display());
        
        match process_rust_project(&project_path, &api_dir) {
            Ok(Some(export)) => {
                println!("Got export statement: {}", export);
                world_exports.push(export);
            },
            Ok(None) => println!("No export statement generated"),
            Err(e) => println!("Error processing project: {}", e),
        }
    }
    
    println!("Collected {} world exports", world_exports.len());
    
    // Check for existing world definition files and update them
    println!("Looking for existing world definition files");
    for entry in WalkDir::new(&api_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        
        if path.is_file() && path.extension().map_or(false, |ext| ext == "wit") {
            println!("Checking WIT file: {}", path.display());
            
            if let Ok(content) = fs::read_to_string(path) {
                if content.contains("world ") {
                    println!("Found world definition file");
                    
                    // Extract the world name
                    let lines: Vec<&str> = content.lines().collect();
                    
                    if let Some(world_line) = lines.iter().find(|line| line.trim().starts_with("world ")) {
                        println!("World line: {}", world_line);
                        
                        if let Some(world_name) = world_line.trim().split_whitespace().nth(1) {
                            let clean_name = world_name.trim_end_matches(" {");
                            println!("Extracted world name: {}", clean_name);
                            
                            // We don't need to validate world names for digits
                            
                            world_names.insert(clean_name.to_string());
                            
                            // Create updated world content - use export instead of import
                            let world_content = format!(
                                "world {} {{\n{}\n    include process-v1;\n}}",
                                clean_name,
                                world_exports.join("\n") // No comma separator because each export has a semicolon
                            );
                            
                            println!("Writing updated world definition to {}", path.display());
                            // Write the updated world file
                            fs::write(path, world_content)
                                .with_context(|| format!("Failed to write updated world file: {}", path.display()))?;
                            
                            println!("Successfully updated world definition");
                        }
                    }
                }
            }
        }
    }
    
    // If no world definitions were found, create a default one
    if world_names.is_empty() && !world_exports.is_empty() {
        // Define default world name
        let default_world = "async-app-template-dot-os-v0";
        println!("No existing world definitions found, creating default with name: {}", default_world);
        
        // We don't need to validate world names for digits
        
        // Create world content with process-v1 include, using export instead of import
        let world_content = format!(
            "world {} {{\n{}\n    include process-v1;\n}}",
            default_world,
            world_exports.join("\n") // No comma separator because each export has a semicolon
        );
        
        let world_file = api_dir.join(format!("{}.wit", default_world));
        println!("Writing default world definition to {}", world_file.display());
        
        fs::write(&world_file, world_content)
            .with_context(|| format!("Failed to write default world file: {}", world_file.display()))?;
        
        println!("Successfully created default world definition");
    }
    
    println!("WIT files generated successfully in the 'api' directory.");
    Ok(())
}

// Find all relevant Rust projects
fn find_rust_projects(base_dir: &Path) -> Vec<PathBuf> {
    let mut projects = Vec::new();
    println!("Scanning for Rust projects in {}", base_dir.display());
    
    for entry in WalkDir::new(base_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        
        if path.is_dir() && path != base_dir {
            let cargo_toml = path.join("Cargo.toml");
            println!("Checking {}", cargo_toml.display());
            
            if cargo_toml.exists() {
                // Try to read and parse Cargo.toml
                if let Ok(content) = fs::read_to_string(&cargo_toml) {
                    if let Ok(cargo_data) = content.parse::<Value>() {
                        // Check for the specific metadata
                        if let Some(metadata) = cargo_data
                            .get("package")
                            .and_then(|p| p.get("metadata"))
                            .and_then(|m| m.get("component"))
                        {
                            if let Some(package) = metadata.get("package") {
                                if let Some(package_str) = package.as_str() {
                                    println!("  Found package.metadata.component.package = {:?}", package_str);
                                    if package_str == "hyperware:process" {
                                        println!("  Adding project: {}", path.display());
                                        projects.push(path.to_path_buf());
                                    }
                                }
                            }
                        } else {
                            println!("  No package.metadata.component metadata found");
                        }
                    }
                }
            }
        }
    }
    
    println!("Found {} relevant Rust projects", projects.len());
    projects
}