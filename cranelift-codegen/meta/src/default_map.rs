use std::collections::HashMap;
use std::hash::Hash;

/// A standard map with an additional get_or_default method that allows to create default entries
/// when a key is looked up for the first time.
pub struct DefaultMap<K, V> {
    map: HashMap<K, V>,
}

impl<K, V> DefaultMap<K, V>
where
    K: Hash + Eq,
    V: Default,
{
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get_or_default(&mut self, k: K) -> &mut V {
        self.map.entry(k).or_insert_with(|| V::default())
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.map.get(k)
    }
}
