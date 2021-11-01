use minidump::Module;
use std::collections::HashMap;
pub use symbols_shim::*;

pub trait SymbolProvider {
    fn fill_symbol(
        &self,
        module: &dyn Module,
        frame: &mut dyn FrameSymbolizer,
    ) -> Result<(), FillSymbolError>;
    fn walk_frame(&self, module: &dyn Module, walker: &mut dyn FrameWalker) -> Option<()>;
    fn stats(&self) -> HashMap<String, SymbolStats>;
}

#[derive(Default)]
pub struct MultiSymbolProvider {
    providers: Vec<Box<dyn SymbolProvider>>,
}

impl MultiSymbolProvider {
    pub fn new() -> MultiSymbolProvider {
        Default::default()
    }

    pub fn add(&mut self, provider: Box<dyn SymbolProvider>) {
        self.providers.push(provider);
    }
}

impl SymbolProvider for MultiSymbolProvider {
    fn fill_symbol(
        &self,
        module: &dyn Module,
        frame: &mut dyn FrameSymbolizer,
    ) -> Result<(), FillSymbolError> {
        // Return Ok if *any* symbol provider came back with Ok, so that the user can
        // distinguish between having no symbols at all and just not being able to
        // symbolize this particular frame.
        let mut best_result = Err(FillSymbolError {});
        for p in self.providers.iter() {
            let new_result = p.fill_symbol(module, frame);
            best_result = best_result.or(new_result);
        }
        best_result
    }

    fn walk_frame(&self, module: &dyn Module, walker: &mut dyn FrameWalker) -> Option<()> {
        for p in self.providers.iter() {
            let result = p.walk_frame(module, walker);
            if result.is_some() {
                return result;
            }
        }
        None
    }

    fn stats(&self) -> HashMap<String, SymbolStats> {
        let mut result = HashMap::new();
        for p in self.providers.iter() {
            // FIXME: do more intelligent merging of the stats
            // (currently doesn't matter as only one provider reports non-empty stats).
            result.extend(p.stats());
        }
        result
    }
}

#[cfg(feature = "breakpad-syms")]
mod symbols_shim {
    use super::SymbolProvider;
    pub use breakpad_symbols::{
        FillSymbolError, FrameSymbolizer, FrameWalker, SymbolStats, SymbolSupplier, Symbolizer,
    };
    use minidump::Module;
    use std::collections::HashMap;
    use std::path::PathBuf;
    impl SymbolProvider for Symbolizer {
        fn fill_symbol(
            &self,
            module: &dyn Module,
            frame: &mut dyn FrameSymbolizer,
        ) -> Result<(), FillSymbolError> {
            self.fill_symbol(module, frame)
        }
        fn walk_frame(&self, module: &dyn Module, walker: &mut dyn FrameWalker) -> Option<()> {
            self.walk_frame(module, walker)
        }
        fn stats(&self) -> HashMap<String, SymbolStats> {
            self.stats()
        }
    }

    /// Gets a SymbolSupplier that looks up symbols by path or with urls.
    ///
    /// May use the `symbols_cache` path to store downloads.
    pub fn http_symbol_supplier(
        symbol_paths: Vec<PathBuf>,
        symbol_urls: Vec<String>,
        symbols_cache: PathBuf,
        symbols_tmp: PathBuf,
    ) -> impl SymbolSupplier {
        breakpad_symbols::HttpSymbolSupplier::new(
            symbol_urls,
            symbols_cache,
            symbols_tmp,
            symbol_paths,
        )
    }

    /// Gets a SymbolSupplier that looks up symbols by path.
    pub fn simple_symbol_supplier(symbol_paths: Vec<PathBuf>) -> impl SymbolSupplier {
        breakpad_symbols::SimpleSymbolSupplier::new(symbol_paths)
    }

    /// Gets a mock SymbolSupplier that just maps module names
    /// to a string containing an entire breakpad .sym file, for tests.
    pub fn string_symbol_supplier(modules: HashMap<String, String>) -> impl SymbolSupplier {
        breakpad_symbols::StringSymbolSupplier::new(modules)
    }
}

#[cfg(feature = "symbolic-syms")]
mod symbols_shim {
    #![allow(dead_code)]

    use super::SymbolProvider;
    use minidump::Module;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // Import symbolic here

    /// A trait for things that can locate symbols for a given module.
    pub trait SymbolSupplier {
        /// Locate and load a symbol file for `module`.
        ///
        /// Implementations may use any strategy for locating and loading
        /// symbols.
        fn locate_symbols(&mut self, module: &dyn Module) -> Result<SymbolFile, SymbolError>;
    }

    /// A trait for setting symbol information on something like a stack frame.
    pub trait FrameSymbolizer {
        /// Get the program counter value for this frame.
        fn get_instruction(&self) -> u64;
        /// Set the name, base address, and paramter size of the function in
        // which this frame is executing.
        fn set_function(&mut self, name: &str, base: u64, parameter_size: u32);
        /// Set the source file and (1-based) line number this frame represents.
        fn set_source_file(&mut self, file: &str, line: u32, base: u64);
    }

    pub trait FrameWalker {
        /// Get the instruction address that we're trying to unwind from.
        fn get_instruction(&self) -> u64;
        /// Get the number of bytes the callee's callee's parameters take up
        /// on the stack (or 0 if unknown/invalid). This is needed for
        /// STACK WIN unwinding.
        fn get_grand_callee_parameter_size(&self) -> u32;
        /// Get a register-sized value stored at this address.
        fn get_register_at_address(&self, address: u64) -> Option<u64>;
        /// Get the value of a register from the callee's frame.
        fn get_callee_register(&self, name: &str) -> Option<u64>;
        /// Set the value of a register for the caller's frame.
        fn set_caller_register(&mut self, name: &str, val: u64) -> Option<()>;
        /// Explicitly mark one of the caller's registers as invalid.
        fn clear_caller_register(&mut self, name: &str);
        /// Set whatever registers in the caller should be set based on the cfa (e.g. rsp).
        fn set_cfa(&mut self, val: u64) -> Option<()>;
        /// Set whatever registers in the caller should be set based on the return address (e.g. rip).
        fn set_ra(&mut self, val: u64) -> Option<()>;
    }

    /// Possible results of locating symbols. (can be opaque, not used externally)
    #[derive(Debug)]
    pub struct SymbolResult;

    /// Symbolicate stack frames.
    ///
    /// A `Symbolizer` manages loading symbols and looking up symbols in them
    /// including caching so that symbols for a given module are only loaded once.
    ///
    /// Call [`Symbolizer::new`][new] to instantiate a `Symbolizer`. A Symbolizer
    /// requires a [`SymbolSupplier`][supplier] to locate symbols. If you have
    /// symbols on disk in the [customary directory layout][dirlayout], a
    /// [`SimpleSymbolSupplier`][simple] will work.
    ///
    /// Use [`get_symbol_at_address`][get_symbol] or [`fill_symbol`][fill_symbol] to
    /// do symbol lookup.
    ///
    /// [new]: struct.Symbolizer.html#method.new
    /// [supplier]: trait.SymbolSupplier.html
    /// [dirlayout]: fn.relative_symbol_path.html
    /// [simple]: struct.SimpleSymbolSupplier.html
    /// [get_symbol]: struct.Symbolizer.html#method.get_symbol_at_address
    /// [fill_symbol]: struct.Symbolizer.html#method.fill_symbol
    pub struct Symbolizer {
        /// Symbol supplier for locating symbols.
        supplier: Box<dyn SymbolSupplier + 'static>,
    }

    impl Symbolizer {
        /// Create a `Symbolizer` that uses `supplier` to locate symbols.
        pub fn new<T: SymbolSupplier + 'static>(supplier: T) -> Symbolizer {
            Symbolizer {
                supplier: Box::new(supplier),
            }
        }
    }

    impl SymbolProvider for Symbolizer {
        fn fill_symbol(
            &self,
            _module: &dyn Module,
            _frame: &mut dyn FrameSymbolizer,
        ) -> Result<(), FillSymbolError> {
            unimplemented!()
        }
        fn walk_frame(&self, _module: &dyn Module, _walker: &mut dyn FrameWalker) -> Option<()> {
            unimplemented!()
        }
    }

    pub struct HttpSymbolSupplier {}

    pub struct SimpleSymbolSupplier {}

    pub struct StringSymbolSupplier {}

    impl SymbolSupplier for HttpSymbolSupplier {
        fn locate_symbols(&self, _module: &dyn Module) -> Result<SymbolFile, SymbolError> {
            unimplemented!()
        }
    }

    impl SymbolSupplier for SimpleSymbolSupplier {
        fn locate_symbols(&self, _module: &dyn Module) -> Result<SymbolFile, SymbolError> {
            unimplemented!()
        }
    }

    impl SymbolSupplier for StringSymbolSupplier {
        fn locate_symbols(&self, _module: &dyn Module) -> Result<SymbolFile, SymbolError> {
            unimplemented!()
        }
    }

    /// Gets a SymbolSupplier that looks up symbols by path or with urls.
    ///
    /// May use the `symbols_cache` path to store downloads.
    pub fn http_symbol_supplier(
        _symbol_paths: Vec<PathBuf>,
        _symbol_urls: Vec<String>,
        _symbols_cache: PathBuf,
        _symbols_tmp: PathBuf,
    ) -> impl SymbolSupplier {
        HttpSymbolSupplier {}
    }

    /// Gets a SymbolSupplier that looks up symbols by path.
    pub fn simple_symbol_supplier(_symbol_paths: Vec<PathBuf>) -> impl SymbolSupplier {
        SimpleSymbolSupplier {}
    }

    /// Gets a mock SymbolSupplier that just maps module names
    /// to a string containing an entire breakpad .sym file, for tests.
    pub fn string_symbol_supplier(_modules: HashMap<String, String>) -> impl SymbolSupplier {
        StringSymbolSupplier {}
    }

    /// Possible results of locating symbols for a module.
    ///
    /// Because symbols may be found from different sources, symbol providers
    /// are usually configured to "cascade" into the next one whenever they report
    /// `NotFound`.
    ///
    /// Cascading currently assumes that if any provider finds symbols for
    /// a module, all other providers will find the same symbols (if any).
    /// Therefore cascading will not be applied if a LoadError or ParseError
    /// occurs (because presumably, all the other sources will also fail to
    /// load/parse.)
    ///
    /// In theory we could do some interesting things where we attempt to
    /// be more robust and actually merge together the symbols from multiple
    /// sources, but that would make it difficult to cache symbol files, and
    /// would rarely actually improve results.
    ///
    /// Since symbol files can be on the order of a gigabyte(!) and downloaded
    /// from the network, aggressive caching is pretty important. The current
    /// approach is a nice balance of simple and effective.
    #[derive(Debug)]
    pub enum SymbolError {
        /// Symbol file could not be found.
        ///
        /// In this case other symbol providers may still be able to find it!
        NotFound,
        /// Symbol file could not be loaded into memory.
        LoadError(Error),
        /// Symbol file was too corrupt to be parsed at all.
        ///
        /// Because symbol files are pretty modular, many corruptions/ambiguities
        /// can be either repaired or discarded at a fairly granular level
        /// (e.g. a bad STACK WIN line can be discarded without affecting anything
        /// else). But sometimes we can't make any sense of the symbol file, and
        /// you find yourself here.
        ParseError(Error),
    }

    #[derive(Debug)]
    pub struct FillSymbolError {}

    // Whatever representation you want, rust-minidump won't look at it.
    struct SymbolFile {}

    /// Statistics on the symbols of a module.
    #[derive(Default, Debug)]
    pub struct SymbolStats {
        /// If the module's symbols were downloaded, this is the url used.
        pub symbol_url: Option<String>,
        /// If the symbols were found and loaded into memory.
        pub loaded_symbols: bool,
        /// If we tried to parse the symbols, but failed.
        pub corrupt_symbols: bool,
    }
}