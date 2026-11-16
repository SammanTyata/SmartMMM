use crate::traits::Module;

/// A constructor for one module. A plain `fn` pointer keeps registration
/// `const`-friendly and free of lifetime or ABI concerns.
pub struct ModuleFactory {
    build: fn() -> Box<dyn Module>,
}

impl ModuleFactory {
    pub fn new(build: fn() -> Box<dyn Module>) -> Self {
        Self { build }
    }

    pub fn build(&self) -> Box<dyn Module> {
        (self.build)()
    }
}

/// The set of modules the core knows how to run.
///
/// v1 registers explicitly (see `bin/smartmmm`). The `inventory` crate can
/// replace this with link-time self-registration later without touching the
/// [`Module`] contract.
#[derive(Default)]
pub struct Registry {
    factories: Vec<ModuleFactory>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, factory: ModuleFactory) {
        self.factories.push(factory);
    }

    pub fn extend(&mut self, factories: impl IntoIterator<Item = ModuleFactory>) {
        self.factories.extend(factories);
    }

    pub fn factories(&self) -> &[ModuleFactory] {
        &self.factories
    }
}
