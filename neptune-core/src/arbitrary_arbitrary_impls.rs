#[cfg(any(test, feature = "arbitrary-impls"))]
use arbitrary::Arbitrary;

#[cfg(any(test, feature = "arbitrary-impls"))]
impl<'a> Arbitrary<'a> for crate::state::wallet::address::pokolen_address::PokolenReceivingAddress {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let seed = tasm_lib::prelude::Digest::arbitrary(u)?;
        Ok(Self::derive_from_seed(seed))
    }
}

#[cfg(any(test, feature = "arbitrary-impls"))]
impl<'a> Arbitrary<'a> for crate::protocol::consensus::transaction::lock_script::LockScript {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let program = tasm_lib::triton_vm::prelude::Program::arbitrary(u)?;
        Ok(Self { program })
    }
}