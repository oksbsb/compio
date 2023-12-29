/// A typed wrapper for key of Ops submitted into driver
#[derive(PartialEq, Eq, Hash)]
pub struct Key<T> {
    user_data: usize,
    _p: std::marker::PhantomData<fn(T)>,
}

impl<T> Unpin for Key<T> {}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

impl<T> Key<T> {
    /// Create a new `Key` with the given user data.
    ///
    /// # Safety
    ///
    /// Caller needs to ensure that `T` does correspond to `user_data` in driver
    /// this `Key` is created with.
    pub unsafe fn new(user_data: usize) -> Self {
        Self {
            user_data,
            _p: std::marker::PhantomData,
        }
    }
}

impl<T> std::ops::Deref for Key<T> {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.user_data
    }
}

impl<T> std::fmt::Debug for Key<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.user_data)
    }
}
