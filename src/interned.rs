#![allow(clippy::undocumented_unsafe_blocks)] // TODO(#697) document safety

use std::any::TypeId;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use dashmap::SharedValue;

use crate::durability::Durability;
use crate::function::VerifyResult;
use crate::hash::FxDashMap;
use crate::ingredient::{fmt_index, Ingredient};
use crate::plumbing::{IngredientIndices, Jar};
use crate::revision::AtomicRevision;
use crate::table::memo::MemoTable;
use crate::table::sync::SyncTable;
use crate::table::Slot;
use crate::zalsa::{IngredientIndex, Zalsa};
use crate::{Database, DatabaseKeyIndex, Event, EventKind, Id, Revision};

pub trait Configuration: Sized + 'static {
    const DEBUG_NAME: &'static str;

    /// The fields of the struct being interned.
    type Fields<'db>: InternedData;

    /// The end user struct
    type Struct<'db>: Copy;

    /// Create an end-user struct from the salsa id
    ///
    /// This call is an "end-step" to the tracked struct lookup/creation
    /// process in a given revision: it occurs only when the struct is newly
    /// created or, if a struct is being reused, after we have updated its
    /// fields (or confirmed it is green and no updates are required).
    fn struct_from_id<'db>(id: Id) -> Self::Struct<'db>;

    /// Deref the struct to yield the underlying id.
    fn deref_struct(s: Self::Struct<'_>) -> Id;
}

pub trait InternedData: Sized + Eq + Hash + Clone + Sync + Send {}
impl<T: Eq + Hash + Clone + Sync + Send> InternedData for T {}

pub struct JarImpl<C: Configuration> {
    phantom: PhantomData<C>,
}

/// The interned ingredient hashes values of type `Data` to produce an `Id`.
///
/// It used to store interned structs but also to store the id fields of a tracked struct.
/// Interned values endure until they are explicitly removed in some way.
pub struct IngredientImpl<C: Configuration> {
    /// Index of this ingredient in the database (used to construct database-ids, etc).
    ingredient_index: IngredientIndex,

    /// Maps from data to the existing interned id for that data.
    ///
    /// Deadlock requirement: We access `value_map` while holding lock on `key_map`, but not vice versa.
    key_map: FxDashMap<C::Fields<'static>, Id>,
}

/// Struct storing the interned fields.
pub struct Value<C>
where
    C: Configuration,
{
    fields: C::Fields<'static>,
    memos: MemoTable,
    syncs: SyncTable,

    /// The revision the value was first interned in.
    first_interned_at: Revision,

    /// The most recent interned revision.
    last_interned_at: AtomicRevision,

    /// The minimum durability of all inputs consumed by the creator
    /// query prior to creating this tracked struct. If any of those
    /// inputs changes, then the creator query may create this struct
    /// with different values.
    durability: AtomicU8,
}

impl<C> Value<C>
where
    C: Configuration,
{
    // Loads the durability of this interned struct.
    fn durability(&self) -> Durability {
        Durability::from_u8(self.durability.load(Ordering::Acquire))
    }

    /// Fields of this interned struct.
    #[cfg(feature = "salsa_unstable")]
    pub fn fields(&self) -> &C::Fields<'static> {
        &self.fields
    }
}

impl<C: Configuration> Default for JarImpl<C> {
    fn default() -> Self {
        Self {
            phantom: PhantomData,
        }
    }
}

impl<C: Configuration> Jar for JarImpl<C> {
    fn create_ingredients(
        _zalsa: &Zalsa,
        first_index: IngredientIndex,
        _dependencies: IngredientIndices,
    ) -> Vec<Box<dyn Ingredient>> {
        vec![Box::new(IngredientImpl::<C>::new(first_index)) as _]
    }

    fn id_struct_type_id() -> TypeId {
        TypeId::of::<C::Struct<'static>>()
    }
}

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    pub fn new(ingredient_index: IngredientIndex) -> Self {
        Self {
            ingredient_index,
            key_map: Default::default(),
        }
    }

    unsafe fn to_internal_data<'db>(&'db self, data: C::Fields<'db>) -> C::Fields<'static> {
        unsafe { std::mem::transmute(data) }
    }

    unsafe fn from_internal_data<'db>(data: &'db C::Fields<'static>) -> &'db C::Fields<'db> {
        unsafe { std::mem::transmute(data) }
    }

    /// Intern data to a unique reference.
    ///
    /// If `key` is already interned, returns the existing [`Id`] for the interned data without
    /// invoking `assemble`.
    /// Otherwise, invokes `assemble` with the given `key` and the [`Id`] to be allocated for this
    /// interned value. The resulting [`C::Data`] will then be interned.
    ///
    /// Note: Using the database within the `assemble` function may result in a deadlock if
    /// the database ends up trying to intern or allocate a new value.
    pub fn intern<'db, Key>(
        &'db self,
        db: &'db dyn crate::Database,
        key: Key,
        assemble: impl FnOnce(Id, Key) -> C::Fields<'db>,
    ) -> C::Struct<'db>
    where
        Key: Hash,
        C::Fields<'db>: HashEqLike<Key>,
    {
        C::struct_from_id(self.intern_id(db, key, assemble))
    }

    /// Intern data to a unique reference.
    ///
    /// If `key` is already interned, returns the existing [`Id`] for the interned data without
    /// invoking `assemble`.
    /// Otherwise, invokes `assemble` with the given `key` and the [`Id`] to be allocated for this
    /// interned value. The resulting [`C::Data`] will then be interned.
    ///
    /// Note: Using the database within the `assemble` function may result in a deadlock if
    /// the database ends up trying to intern or allocate a new value.
    pub fn intern_id<'db, Key>(
        &'db self,
        db: &'db dyn crate::Database,
        key: Key,
        assemble: impl FnOnce(Id, Key) -> C::Fields<'db>,
    ) -> crate::Id
    where
        Key: Hash,
        // We'd want the following predicate, but this currently implies `'static` due to a rustc
        // bug
        // for<'db> C::Data<'db>: HashEqLike<Key>,
        // so instead we go with this and transmute the lifetime in the `eq` closure
        C::Fields<'db>: HashEqLike<Key>,
    {
        let (zalsa, zalsa_local) = db.zalsas();
        let current_revision = zalsa.current_revision();

        // Optimization to only get read lock on the map if the data has already been interned.
        let data_hash = self.key_map.hasher().hash_one(&key);
        let shard = &self.key_map.shards()[self.key_map.determine_shard(data_hash as _)];
        let eq = |(data, _): &_| {
            // SAFETY: it's safe to go from Data<'static> to Data<'db>
            // shrink lifetime here to use a single lifetime in Lookup::eq(&StructKey<'db>, &C::Data<'db>)
            let data: &C::Fields<'db> = unsafe { std::mem::transmute(data) };
            HashEqLike::eq(data, &key)
        };

        {
            let lock = shard.read();
            if let Some(bucket) = lock.find(data_hash, eq) {
                // SAFETY: Read lock on map is held during this block
                let id = unsafe { *bucket.as_ref().1.get() };

                let value = zalsa.table().get::<Value<C>>(id);

                // Sync the value's revision.
                if value.last_interned_at.load() < current_revision {
                    value.last_interned_at.store(current_revision);
                    db.salsa_event(&|| {
                        Event::new(EventKind::DidReinternValue {
                            id,
                            revision: current_revision,
                        })
                    });
                }

                let durability = if let Some((_, stamp)) = zalsa_local.active_query() {
                    // Record the maximum durability across all queries that intern this value.
                    let previous_durability = value
                        .durability
                        .fetch_max(stamp.durability.as_u8(), Ordering::AcqRel);

                    Durability::from_u8(previous_durability).max(stamp.durability)
                } else {
                    value.durability()
                };

                // Record a dependency on this value.
                let index = self.database_key_index(id);
                zalsa_local.report_tracked_read_simple(index, durability, value.first_interned_at);

                return id;
            }
        }

        let mut lock = shard.write();
        match lock.find_or_find_insert_slot(data_hash, eq, |(element, _)| {
            self.key_map.hasher().hash_one(element)
        }) {
            // Data has been interned by a racing call, use that ID instead
            Ok(slot) => {
                let id = unsafe { *slot.as_ref().1.get() };
                let value = zalsa.table().get::<Value<C>>(id);

                // Sync the value's revision.
                if value.last_interned_at.load() < current_revision {
                    value.last_interned_at.store(current_revision);
                    db.salsa_event(&|| {
                        Event::new(EventKind::DidReinternValue {
                            id,
                            revision: current_revision,
                        })
                    });
                }

                let durability = if let Some((_, stamp)) = zalsa_local.active_query() {
                    // Record the maximum durability across all queries that intern this value.
                    let previous_durability = value
                        .durability
                        .fetch_max(stamp.durability.as_u8(), Ordering::AcqRel);

                    Durability::from_u8(previous_durability).max(stamp.durability)
                } else {
                    value.durability()
                };

                // Record a dependency on this value.
                let index = self.database_key_index(id);
                zalsa_local.report_tracked_read_simple(index, durability, value.first_interned_at);

                id
            }

            // We won any races so should intern the data
            Err(slot) => {
                let table = zalsa.table();

                // Record the durability of the current query on the interned value.
                let durability = zalsa_local
                    .active_query()
                    .map(|(_, stamp)| stamp.durability)
                    // If there is no active query this durability does not actually matter.
                    .unwrap_or(Durability::MAX);

                let id = zalsa_local.allocate(table, self.ingredient_index, |id| Value::<C> {
                    fields: unsafe { self.to_internal_data(assemble(id, key)) },
                    memos: Default::default(),
                    syncs: Default::default(),
                    durability: AtomicU8::new(durability.as_u8()),
                    // Record the revision we are interning in.
                    first_interned_at: current_revision,
                    last_interned_at: AtomicRevision::from(current_revision),
                });

                let value = table.get::<Value<C>>(id);

                let slot_value = (value.fields.clone(), SharedValue::new(id));
                unsafe { lock.insert_in_slot(data_hash, slot, slot_value) };

                debug_assert_eq!(
                    data_hash,
                    self.key_map
                        .hasher()
                        .hash_one(table.get::<Value<C>>(id).fields.clone())
                );

                // Record a dependency on this value.
                let index = self.database_key_index(id);
                zalsa_local.report_tracked_read_simple(index, durability, value.first_interned_at);

                db.salsa_event(&|| {
                    Event::new(EventKind::DidInternValue {
                        id,
                        revision: current_revision,
                    })
                });

                id
            }
        }
    }

    /// Returns the database key index for an interned value with the given id.
    pub fn database_key_index(&self, id: Id) -> DatabaseKeyIndex {
        DatabaseKeyIndex::new(self.ingredient_index, id)
    }

    /// Lookup the data for an interned value based on its id.
    /// Rarely used since end-users generally carry a struct with a pointer directly
    /// to the interned item.
    pub fn data<'db>(&'db self, db: &'db dyn Database, id: Id) -> &'db C::Fields<'db> {
        let zalsa = db.zalsa();
        let internal_data = zalsa.table().get::<Value<C>>(id);
        let last_changed_revision = zalsa.last_changed_revision(internal_data.durability());

        assert!(
            internal_data.last_interned_at.load() >= last_changed_revision,
            "Data was not interned in the latest revision for its durability."
        );

        unsafe { Self::from_internal_data(&internal_data.fields) }
    }

    /// Lookup the fields from an interned struct.
    /// Note that this is not "leaking" since no dependency edge is required.
    pub fn fields<'db>(&'db self, db: &'db dyn Database, s: C::Struct<'db>) -> &'db C::Fields<'db> {
        self.data(db, C::deref_struct(s))
    }

    pub fn reset(&mut self, db: &mut dyn Database) {
        db.zalsa_mut().new_revision();
        self.key_map.clear();
    }

    #[cfg(feature = "salsa_unstable")]
    /// Returns all data corresponding to the interned struct.
    pub fn entries<'db>(
        &'db self,
        db: &'db dyn crate::Database,
    ) -> impl Iterator<Item = &'db Value<C>> {
        db.zalsa().table().slots_of::<Value<C>>()
    }
}

impl<C> Ingredient for IngredientImpl<C>
where
    C: Configuration,
{
    fn ingredient_index(&self) -> IngredientIndex {
        self.ingredient_index
    }

    unsafe fn maybe_changed_after(
        &self,
        db: &dyn Database,
        input: Id,
        revision: Revision,
    ) -> VerifyResult {
        let zalsa = db.zalsa();
        let value = zalsa.table().get::<Value<C>>(input);
        if value.first_interned_at > revision {
            // The slot was reused.
            return VerifyResult::Changed;
        }

        // The slot is valid in this revision but we have to sync the value's revision.
        let current_revision = zalsa.current_revision();
        value.last_interned_at.store(current_revision);

        db.salsa_event(&|| {
            Event::new(EventKind::DidReinternValue {
                id: input,
                revision: current_revision,
            })
        });

        VerifyResult::unchanged()
    }

    fn fmt_index(&self, index: crate::Id, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_index(C::DEBUG_NAME, index, fmt)
    }

    fn debug_name(&self) -> &'static str {
        C::DEBUG_NAME
    }
}

impl<C> std::fmt::Debug for IngredientImpl<C>
where
    C: Configuration,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("index", &self.ingredient_index)
            .finish()
    }
}

impl<C> Slot for Value<C>
where
    C: Configuration,
{
    #[inline(always)]
    unsafe fn memos(&self, _current_revision: Revision) -> &MemoTable {
        &self.memos
    }

    #[inline(always)]
    fn memos_mut(&mut self) -> &mut MemoTable {
        &mut self.memos
    }

    #[inline(always)]
    unsafe fn syncs(&self, _current_revision: Revision) -> &crate::table::sync::SyncTable {
        &self.syncs
    }
}

/// A trait for types that hash and compare like `O`.
pub trait HashEqLike<O> {
    fn hash<H: Hasher>(&self, h: &mut H);
    fn eq(&self, data: &O) -> bool;
}

/// The `Lookup` trait is a more flexible variant on [`std::borrow::Borrow`]
/// and [`std::borrow::ToOwned`].
///
/// It is implemented by "some type that can be used as the lookup key for `O`".
/// This means that `self` can be hashed and compared for equality with values
/// of type `O` without actually creating an owned value. It `self` needs to be interned,
/// it can be converted into an equivalent value of type `O`.
///
/// The canonical example is `&str: Lookup<String>`. However, this example
/// alone can be handled by [`std::borrow::Borrow`][]. In our case, we may have
/// multiple keys accumulated into a struct, like `ViewStruct: Lookup<(K1, ...)>`,
/// where `struct ViewStruct<L1: Lookup<K1>...>(K1...)`. The `Borrow` trait
/// requires that `&(K1...)` be convertible to `&ViewStruct` which just isn't
/// possible. `Lookup` instead offers direct `hash` and `eq` methods.
pub trait Lookup<O> {
    fn into_owned(self) -> O;
}

impl<T> Lookup<T> for T {
    fn into_owned(self) -> T {
        self
    }
}

impl<T> HashEqLike<T> for T
where
    T: Hash + Eq,
{
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, &mut *h);
    }

    fn eq(&self, data: &T) -> bool {
        self == data
    }
}

impl<T> HashEqLike<T> for &T
where
    T: Hash + Eq,
{
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(*self, &mut *h);
    }

    fn eq(&self, data: &T) -> bool {
        **self == *data
    }
}

impl<T> HashEqLike<&T> for T
where
    T: Hash + Eq,
{
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, &mut *h);
    }

    fn eq(&self, data: &&T) -> bool {
        *self == **data
    }
}

impl<T> Lookup<T> for &T
where
    T: Clone,
{
    fn into_owned(self) -> T {
        Clone::clone(self)
    }
}

impl<'a, T> HashEqLike<&'a T> for Box<T>
where
    T: ?Sized + Hash + Eq,
    Box<T>: From<&'a T>,
{
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, &mut *h)
    }
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T> Lookup<Box<T>> for &'a T
where
    T: ?Sized + Hash + Eq,
    Box<T>: From<&'a T>,
{
    fn into_owned(self) -> Box<T> {
        Box::from(self)
    }
}

impl<'a, T> HashEqLike<&'a T> for Arc<T>
where
    T: ?Sized + Hash + Eq,
    Arc<T>: From<&'a T>,
{
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, &mut *h)
    }
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T> Lookup<Arc<T>> for &'a T
where
    T: ?Sized + Hash + Eq,
    Arc<T>: From<&'a T>,
{
    fn into_owned(self) -> Arc<T> {
        Arc::from(self)
    }
}

impl Lookup<String> for &str {
    fn into_owned(self) -> String {
        self.to_owned()
    }
}
impl HashEqLike<&str> for String {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, &mut *h)
    }

    fn eq(&self, data: &&str) -> bool {
        self == *data
    }
}

impl<A, T: Hash + Eq + PartialEq<A>> HashEqLike<&[A]> for Vec<T> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, h);
    }

    fn eq(&self, data: &&[A]) -> bool {
        self.len() == data.len() && data.iter().enumerate().all(|(i, a)| &self[i] == a)
    }
}
impl<A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for &[A] {
    fn into_owned(self) -> Vec<T> {
        self.iter().map(|a| Lookup::into_owned(a.clone())).collect()
    }
}

impl<const N: usize, A, T: Hash + Eq + PartialEq<A>> HashEqLike<[A; N]> for Vec<T> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, h);
    }

    fn eq(&self, data: &[A; N]) -> bool {
        self.len() == data.len() && data.iter().enumerate().all(|(i, a)| &self[i] == a)
    }
}
impl<const N: usize, A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for [A; N] {
    fn into_owned(self) -> Vec<T> {
        self.into_iter()
            .map(|a| Lookup::into_owned(a.clone()))
            .collect()
    }
}

impl HashEqLike<&Path> for PathBuf {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hash::hash(self, h);
    }

    fn eq(&self, data: &&Path) -> bool {
        self == data
    }
}
impl Lookup<PathBuf> for &Path {
    fn into_owned(self) -> PathBuf {
        self.to_owned()
    }
}
