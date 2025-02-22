//! Implement a registry of function signatures, for fast indirect call
//! signature checking.

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::RwLock,
};
use std::{convert::TryFrom, sync::Arc};
use wasmtime_environ::{ModuleTypes, PrimaryMap, SignatureIndex, WasmFuncType};
use wasmtime_runtime::VMSharedSignatureIndex;

/// Represents a collection of shared signatures.
///
/// This is used to register shared signatures with a shared signature registry.
///
/// The collection will unregister any contained signatures with the registry
/// when dropped.
#[derive(Debug)]
pub struct SignatureCollection {
    registry: Arc<RwLock<SignatureRegistryInner>>,
    signatures: PrimaryMap<SignatureIndex, VMSharedSignatureIndex>,
    reverse_signatures: HashMap<VMSharedSignatureIndex, SignatureIndex>,
}

impl SignatureCollection {
    /// Creates a signature collection for a module given the module's signatures.
    pub fn new_for_module(registry: &SignatureRegistry, types: &ModuleTypes) -> Self {
        let signatures = registry.0.write().unwrap().register_for_module(types);
        let reverse_signatures = signatures.iter().map(|(k, v)| (*v, k)).collect();

        Self {
            registry: registry.0.clone(),
            signatures,
            reverse_signatures,
        }
    }

    /// Treats the signature collection as a map from a module signature index to
    /// registered shared signature indexes.
    ///
    /// This is used for looking up module shared signature indexes during module
    /// instantiation.
    pub fn as_module_map(&self) -> &PrimaryMap<SignatureIndex, VMSharedSignatureIndex> {
        &self.signatures
    }

    /// Gets the shared signature index given a module signature index.
    #[inline]
    pub fn shared_signature(&self, index: SignatureIndex) -> Option<VMSharedSignatureIndex> {
        self.signatures.get(index).copied()
    }

    /// Get the module-local signature index for the given shared signature index.
    pub fn local_signature(&self, index: VMSharedSignatureIndex) -> Option<SignatureIndex> {
        self.reverse_signatures.get(&index).copied()
    }
}

impl Drop for SignatureCollection {
    fn drop(&mut self) {
        if !self.signatures.is_empty() {
            self.registry.write().unwrap().unregister_signatures(self);
        }
    }
}

#[derive(Debug)]
struct RegistryEntry {
    references: usize,
    ty: WasmFuncType,
}

#[derive(Debug, Default)]
struct SignatureRegistryInner {
    // A map from the Wasm function type to a `VMSharedSignatureIndex`, for all
    // the Wasm function types we have already registered.
    map: HashMap<WasmFuncType, VMSharedSignatureIndex>,

    // A map from `VMSharedSignatureIndex::bits()` to the signature index's
    // associated data, such as the underlying Wasm type.
    entries: Vec<Option<RegistryEntry>>,

    // A free list of the `VMSharedSignatureIndex`es that are no longer being
    // used by anything, and can therefore be reused.
    //
    // This is a size optimization, and not strictly necessary for correctness:
    // we reuse entries rather than leak them and have logical holes in our
    // `self.entries` list.
    free: Vec<VMSharedSignatureIndex>,
}

impl SignatureRegistryInner {
    fn register_for_module(
        &mut self,
        types: &ModuleTypes,
    ) -> PrimaryMap<SignatureIndex, VMSharedSignatureIndex> {
        let mut sigs = PrimaryMap::default();
        for (idx, ty) in types.wasm_signatures() {
            let b = sigs.push(self.register(ty));
            assert_eq!(idx, b);
        }
        sigs
    }

    fn register(&mut self, ty: &WasmFuncType) -> VMSharedSignatureIndex {
        let len = self.map.len();

        let index = match self.map.entry(ty.clone()) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let (index, entry) = match self.free.pop() {
                    Some(index) => (index, &mut self.entries[index.bits() as usize]),
                    None => {
                        // Keep `index_map`'s length under `u32::MAX` because
                        // `u32::MAX` is reserved for `VMSharedSignatureIndex`'s
                        // default value.
                        assert!(
                            len < std::u32::MAX as usize,
                            "Invariant check: index_map.len() < std::u32::MAX"
                        );
                        debug_assert_eq!(len, self.entries.len());

                        let index = VMSharedSignatureIndex::new(u32::try_from(len).unwrap());
                        self.entries.push(None);

                        (index, self.entries.last_mut().unwrap())
                    }
                };

                // The entry should be missing for one just allocated or
                // taken from the free list
                assert!(entry.is_none());

                *entry = Some(RegistryEntry {
                    references: 0,
                    ty: ty.clone(),
                });

                *e.insert(index)
            }
        };

        self.entries[index.bits() as usize]
            .as_mut()
            .unwrap()
            .references += 1;

        index
    }

    fn unregister_signatures(&mut self, collection: &SignatureCollection) {
        for (_, index) in collection.signatures.iter() {
            self.unregister_entry(*index, 1);
        }
    }

    fn unregister_entry(&mut self, index: VMSharedSignatureIndex, count: usize) {
        let removed = {
            let entry = self.entries[index.bits() as usize].as_mut().unwrap();

            debug_assert!(entry.references >= count);
            entry.references -= count;

            if entry.references == 0 {
                self.map.remove(&entry.ty);
                self.free.push(index);
                true
            } else {
                false
            }
        };

        if removed {
            self.entries[index.bits() as usize] = None;
        }
    }
}

// `SignatureRegistryInner` implements `Drop` in debug builds to assert that
// all signatures have been unregistered for the registry.
#[cfg(debug_assertions)]
impl Drop for SignatureRegistryInner {
    fn drop(&mut self) {
        assert!(
            self.map.is_empty(),
            "signature registry not empty: still have registered types in self.map"
        );
        assert_eq!(
            self.free.len(),
            self.entries.len(),
            "signature registery not empty: not all entries in free list"
        );
    }
}

/// Implements a shared signature registry.
///
/// WebAssembly requires that the caller and callee signatures in an indirect
/// call must match. To implement this efficiently, keep a registry of all
/// signatures, shared by all instances, so that call sites can just do an
/// index comparison.
#[derive(Debug)]
pub struct SignatureRegistry(Arc<RwLock<SignatureRegistryInner>>);

impl SignatureRegistry {
    /// Creates a new shared signature registry.
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(SignatureRegistryInner::default())))
    }

    /// Looks up a function type from a shared signature index.
    pub fn lookup_type(&self, index: VMSharedSignatureIndex) -> Option<WasmFuncType> {
        self.0
            .read()
            .unwrap()
            .entries
            .get(index.bits() as usize)
            .and_then(|e| e.as_ref().map(|e| &e.ty).cloned())
    }

    /// Registers a single function with the collection.
    ///
    /// Returns the shared signature index for the function.
    pub fn register(&self, ty: &WasmFuncType) -> VMSharedSignatureIndex {
        self.0.write().unwrap().register(ty)
    }

    /// Registers a single function with the collection.
    ///
    /// Returns the shared signature index for the function.
    pub unsafe fn unregister(&self, sig: VMSharedSignatureIndex) {
        self.0.write().unwrap().unregister_entry(sig, 1)
    }
}
