//! Structs for building transactions.

use std::error;
use std::fmt;
use std::sync::mpsc::Sender;

#[cfg(not(feature = "zfuture"))]
use std::marker::PhantomData;

use rand::{rngs::OsRng, CryptoRng, RngCore};

use crate::{
    consensus::{self, BlockHeight, BranchId},
    keys::OutgoingViewingKey,
    legacy::TransparentAddress,
    memo::MemoBytes,
    merkle_tree::MerklePath,
    sapling::{prover::TxProver, Diversifier, Node, Note, PaymentAddress},
    transaction::{
        components::{
            amount::{Amount, DEFAULT_FEE},
            orchard::builder::{NoOrchardBuilder, OrchardBuilder, UseOrchard},
            sapling::{
                self,
                builder::{SaplingBuilder, SaplingMetadata},
            },
            transparent::{self, builder::TransparentBuilder},
        },
        sighash::{signature_hash, SignableInput},
        txid::TxIdDigester,
        Transaction, TransactionData, TxVersion, Unauthorized,
    },
    zip32::ExtendedSpendingKey,
};

#[cfg(feature = "transparent-inputs")]
use crate::transaction::components::transparent::TxOut;

#[cfg(feature = "zfuture")]
use crate::{
    extensions::transparent::{ExtensionTxBuilder, ToPayload},
    transaction::components::{
        tze::builder::TzeBuilder,
        tze::{self, TzeOut},
    },
};

#[cfg(any(test, feature = "test-dependencies"))]
use crate::sapling::prover::mock::MockTxProver;

const DEFAULT_TX_EXPIRY_DELTA: u32 = 20;

#[derive(Debug)]
pub enum Error {
    ChangeIsNegative(Amount),
    InvalidAmount,
    NoChangeAddress,
    TransparentBuild(transparent::builder::Error),
    SaplingBuild(sapling::builder::Error),
    OrchardBuild(orchard::builder::Error),
    OrchardComponent(&'static str),
    NU5Inactive,
    #[cfg(feature = "zfuture")]
    TzeBuild(tze::builder::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ChangeIsNegative(amount) => {
                write!(f, "Change is negative ({:?} zatoshis)", amount)
            }
            Error::InvalidAmount => write!(f, "Invalid amount"),
            Error::NoChangeAddress => write!(f, "No change address specified or discoverable"),
            Error::TransparentBuild(err) => err.fmt(f),
            Error::SaplingBuild(err) => err.fmt(f),
            Error::OrchardBuild(err) => write!(f, "{:?}", err),
            Error::OrchardComponent(err) => write!(f, "Could not add orchard component: {}", err),
            Error::NU5Inactive => write!(
                f,
                "Cannot create orchard transactions before NU5 activation"
            ),
            #[cfg(feature = "zfuture")]
            Error::TzeBuild(err) => err.fmt(f),
        }
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::ChangeIsNegative(e), Error::ChangeIsNegative(f)) => e == f,
            (Error::InvalidAmount, Error::InvalidAmount) => true,
            (Error::NoChangeAddress, Error::NoChangeAddress) => true,
            (Error::TransparentBuild(e), Error::TransparentBuild(f)) => e == f,
            (Error::SaplingBuild(e), Error::SaplingBuild(f)) => e == f,
            (Error::OrchardBuild(e), Error::OrchardBuild(f)) => {
                format!("{:?}", e) == format!("{:?}", f)
            }
            (Error::OrchardComponent(e), Error::OrchardComponent(f)) => e == f,
            (Error::NU5Inactive, Error::NU5Inactive) => true,
            #[cfg(feature = "zfuture")]
            (Error::TzeBuild(e), Error::TzeBuild(f)) => e == f,
            _ => false,
        }
    }
}

impl error::Error for Error {}

/// Reports on the progress made by the builder towards building a transaction.
pub struct Progress {
    /// The number of steps completed.
    cur: u32,
    /// The expected total number of steps (as of this progress update), if known.
    end: Option<u32>,
}

impl Progress {
    pub fn new(cur: u32, end: Option<u32>) -> Self {
        Self { cur, end }
    }

    /// Returns the number of steps completed so far while building the transaction.
    ///
    /// Note that each step may not be of the same complexity/duration.
    pub fn cur(&self) -> u32 {
        self.cur
    }

    /// Returns the total expected number of steps before this transaction will be ready,
    /// or `None` if the end is unknown as of this progress update.
    ///
    /// Note that each step may not be of the same complexity/duration.
    pub fn end(&self) -> Option<u32> {
        self.end
    }
}

enum ChangeAddress {
    SaplingChangeAddress(OutgoingViewingKey, PaymentAddress),
}

/// Generates a [`Transaction`] from its inputs and outputs.
pub struct Builder<'a, P, R, O: UseOrchard = NoOrchardBuilder> {
    params: P,
    rng: R,
    target_height: BlockHeight,
    expiry_height: BlockHeight,
    fee: Amount,
    transparent_builder: TransparentBuilder,
    sapling_builder: SaplingBuilder<P>,
    contains_orchard: bool,
    orchard_builder: O,
    orchard_spending_keys: Vec<orchard::keys::SpendAuthorizingKey>, // We need these for signatures
    change_address: Option<ChangeAddress>,
    #[cfg(feature = "zfuture")]
    tze_builder: TzeBuilder<'a, TransactionData<Unauthorized>>,
    #[cfg(not(feature = "zfuture"))]
    tze_builder: PhantomData<&'a ()>,
    progress_notifier: Option<Sender<Progress>>,
}

impl<'a, P: consensus::Parameters> Builder<'a, P, OsRng> {
    /// Creates a new `Builder` targeted for inclusion in the block with the given height,
    /// using default values for general transaction fields and the default OS random.
    ///
    /// # Default values
    ///
    /// The expiry height will be set to the given height plus the default transaction
    /// expiry delta (20 blocks).
    ///
    /// The fee will be set to the default fee (0.0001 ZEC).
    pub fn new(params: P, target_height: BlockHeight) -> Self {
        Builder::new_with_rng(params, target_height, OsRng)
    }
}

impl<'a, P: consensus::Parameters> Builder<'a, P, OsRng, OrchardBuilder> {
    /// Creates a new `Builder` targeted for inclusion in the block with the given height,
    /// using default values for general transaction fields and the default OS random.
    ///
    /// # Default values
    ///
    /// The expiry height will be set to the given height plus the default transaction
    /// expiry delta (20 blocks).
    ///
    /// The fee will be set to the default fee (0.0001 ZEC).
    pub fn new_with_orchard(
        params: P,
        target_height: BlockHeight,
        anchor: orchard::tree::Anchor,
    ) -> Self {
        let with_orchard = OrchardBuilder::new(&params, target_height, anchor);
        Builder::new_internal(params, target_height, OsRng, with_orchard)
    }
}

impl<'a, P: consensus::Parameters, R: RngCore + CryptoRng> Builder<'a, P, R> {
    /// Creates a new `Builder` targeted for inclusion in the block with the given height
    /// and randomness source, using default values for general transaction fields.
    ///
    /// # Default values
    ///
    /// The expiry height will be set to the given height plus the default transaction
    /// expiry delta (20 blocks).
    ///
    /// The fee will be set to the default fee (0.0001 ZEC).
    pub fn new_with_rng(params: P, target_height: BlockHeight, rng: R) -> Builder<'a, P, R> {
        Self::new_internal(params, target_height, rng, NoOrchardBuilder)
    }
}

impl<'a, P: consensus::Parameters, R: RngCore + CryptoRng, O: UseOrchard> Builder<'a, P, R, O> {
    /// Common utility function for builder construction.
    fn new_internal(params: P, target_height: BlockHeight, rng: R, orchard_builder: O) -> Self {
        Builder {
            params: params.clone(),
            rng,
            target_height,
            expiry_height: target_height + DEFAULT_TX_EXPIRY_DELTA,
            fee: DEFAULT_FEE,
            contains_orchard: false,
            transparent_builder: TransparentBuilder::empty(),
            sapling_builder: SaplingBuilder::new(params, target_height),
            orchard_builder,
            orchard_spending_keys: Vec::new(),
            change_address: None,
            #[cfg(feature = "zfuture")]
            tze_builder: TzeBuilder::empty(),
            #[cfg(not(feature = "zfuture"))]
            tze_builder: PhantomData,
            progress_notifier: None,
        }
    }
}

impl<'a, P: consensus::Parameters, R: RngCore + CryptoRng> Builder<'a, P, R, OrchardBuilder> {
    /// Adds a note to be spent in this transaction.
    ///
    /// - `note` is a spendable note, obtained by trial-decrypting an [`Action`] using the
    ///   [`zcash_note_encryption`] crate instantiated with [`OrchardDomain`].
    /// - `merkle_path` can be obtained using the [`incrementalmerkletree`] crate
    ///   instantiated with [`MerkleHashOrchard`].
    ///
    /// Returns an error if the given Merkle path does not have the required anchor for
    /// the given note.
    ///
    /// [`OrchardDomain`]: crate::note_encryption::OrchardDomain
    /// [`MerkleHashOrchard`]: crate::tree::MerkleHashOrchard
    pub fn add_orchard_spend(
        &mut self,
        sk: orchard::keys::SpendingKey,
        note: orchard::Note,
        merkle_path: orchard::tree::MerklePath,
    ) -> Result<(), Error> {
        self.contains_orchard = true;

        self.orchard_spending_keys
            .push(orchard::keys::SpendAuthorizingKey::from(&sk));

        self.orchard_builder
            .0
            .as_mut()
            .ok_or(Error::NU5Inactive)?
            .add_spend(orchard::keys::FullViewingKey::from(&sk), note, merkle_path)
            .map_err(Error::OrchardComponent)
    }

    /// Adds an address which will receive funds in this transaction.
    pub fn add_orchard_output(
        &mut self,
        ovk: Option<orchard::keys::OutgoingViewingKey>,
        recipient: orchard::Address,
        value: u64,
        memo: MemoBytes,
    ) -> Result<(), Error> {
        self.contains_orchard = true;

        self.orchard_builder
            .0
            .as_mut()
            .ok_or(Error::NU5Inactive)?
            .add_recipient(
                ovk,
                recipient,
                orchard::value::NoteValue::from_raw(value),
                Some(*memo.as_array()),
            )
            .map_err(Error::OrchardComponent)
    }
}

impl<'a, P: consensus::Parameters, R: RngCore + CryptoRng, O: UseOrchard> Builder<'a, P, R, O> {
    /// Adds a Sapling note to be spent in this transaction.
    ///
    /// Returns an error if the given Merkle path does not have the same anchor as the
    /// paths for previous Sapling notes.
    pub fn add_sapling_spend(
        &mut self,
        extsk: ExtendedSpendingKey,
        diversifier: Diversifier,
        note: Note,
        merkle_path: MerklePath<Node>,
    ) -> Result<(), Error> {
        self.sapling_builder
            .add_spend(&mut self.rng, extsk, diversifier, note, merkle_path)
            .map_err(Error::SaplingBuild)
    }

    /// Adds a Sapling address to send funds to.
    pub fn add_sapling_output(
        &mut self,
        ovk: Option<OutgoingViewingKey>,
        to: PaymentAddress,
        value: Amount,
        memo: MemoBytes,
    ) -> Result<(), Error> {
        self.sapling_builder
            .add_output(&mut self.rng, ovk, to, value, memo)
            .map_err(Error::SaplingBuild)
    }

    /// Adds a transparent coin to be spent in this transaction.
    #[cfg(feature = "transparent-inputs")]
    #[cfg_attr(docsrs, doc(cfg(feature = "transparent-inputs")))]
    pub fn add_transparent_input(
        &mut self,
        sk: secp256k1::SecretKey,
        utxo: transparent::OutPoint,
        coin: TxOut,
    ) -> Result<(), Error> {        
        self.transparent_builder
            .add_input(sk, utxo, coin)
            .map_err(Error::TransparentBuild)
    }

    /// Adds a transparent address to send funds to.
    pub fn add_transparent_output(
        &mut self,
        to: &TransparentAddress,
        value: Amount,
    ) -> Result<(), Error> {
        self.transparent_builder
            .add_output(to, value)
            .map_err(Error::TransparentBuild)
    }

    /// Sets the Sapling address to which any change will be sent.
    ///
    /// By default, change is sent to the Sapling address corresponding to the first note
    /// being spent (i.e. the first call to [`Builder::add_sapling_spend`]).
    pub fn send_change_to(&mut self, ovk: OutgoingViewingKey, to: PaymentAddress) {
        self.change_address = Some(ChangeAddress::SaplingChangeAddress(ovk, to))
    }

    /// Sets the notifier channel, where progress of building the transaction is sent.
    ///
    /// An update is sent after every Spend or Output is computed, and the `u32` sent
    /// represents the total steps completed so far. It will eventually send number of
    /// spends + outputs. If there's an error building the transaction, the channel is
    /// closed.
    pub fn with_progress_notifier(&mut self, progress_notifier: Sender<Progress>) {
        self.progress_notifier = Some(progress_notifier);
    }

    /// Returns the sum of the transparent, Sapling, Orchard, and TZE value balances.
    fn value_balance(&self) -> Result<Amount, Error> {
        let value_balances = [
            self.transparent_builder
                .value_balance()
                .ok_or(Error::InvalidAmount)?,
            self.sapling_builder.value_balance(),
            Amount::from_i64(self.orchard_builder.value_balance())
                .map_err(|_| Error::InvalidAmount)?,
            // self.fee,
            #[cfg(feature = "zfuture")]
            self.tze_builder
                .value_balance()
                .ok_or(Error::InvalidAmount)?,
        ];

        IntoIterator::into_iter(&value_balances)
            .sum::<Option<_>>()
            .ok_or(Error::InvalidAmount)
    }

    /// Set the custom fee
    pub fn set_custom_fee(&mut self, custom_fee: Amount) {
        self.fee = custom_fee;
    }

    /// Builds a transaction from the configured spends and outputs.
    ///
    /// Upon success, returns a tuple containing the final transaction, and the
    /// [`SaplingMetadata`] generated during the build process.
    pub fn build(
        mut self,
        prover: &impl TxProver
    ) -> Result<(Transaction, SaplingMetadata), Error> {
        let consensus_branch_id = BranchId::for_height(&self.params, self.target_height);

        // determine transaction version
        let version = TxVersion::suggested_for_branch(consensus_branch_id);

        //
        // Consistency checks
        //

        // Valid change
        let change = (self.value_balance()? - self.fee).ok_or(Error::InvalidAmount)?;
        println!("ValueBalance: {}", u64::from(self.value_balance()?));
        println!("fee: {}", u64::from(self.fee));

        if change.is_negative() {
            println!("ValueBalance: {}", u64::from(self.value_balance()?));
            return Err(Error::ChangeIsNegative(change));
        }

        //
        // Change output
        //

        if change.is_positive() {
            // Send change to the specified change address. If no change address
            // was set, send change to the first Sapling address given as input.
            match self.change_address.take() {
                Some(ChangeAddress::SaplingChangeAddress(ovk, addr)) => {
                    self.add_sapling_output(Some(ovk), addr, change, MemoBytes::empty())?;
                }
                None => {
                    let (ovk, addr) = self
                        .sapling_builder
                        .get_candidate_change_address()
                        .ok_or(Error::NoChangeAddress)?;
                    self.add_sapling_output(Some(ovk), addr, change, MemoBytes::empty())?;
                }
            }
        }

        let transparent_bundle = self.transparent_builder.build();

        let mut rng = self.rng;
        let mut ctx = prover.new_sapling_proving_context();
        let sapling_bundle = self
            .sapling_builder
            .build(
                prover,
                &mut ctx,
                &mut rng,
                self.target_height,
                self.progress_notifier.as_ref(),
            )
            .map_err(Error::SaplingBuild)?;

        // Build orchard only if there are any items in the orchard bundle.
        let orchard_bundle: Option<orchard::Bundle<_, Amount>> = if self.contains_orchard {
            self.orchard_builder
                .build(&mut rng)
                .transpose()
                .map_err(Error::OrchardBuild)?
        } else {
            None
        };

        #[cfg(feature = "zfuture")]
        let (tze_bundle, tze_signers) = self.tze_builder.build();

        let unauthed_tx: TransactionData<Unauthorized> = TransactionData {
            version,
            consensus_branch_id: BranchId::for_height(&self.params, self.target_height),
            lock_time: 0,
            expiry_height: self.expiry_height,
            transparent_bundle,
            sprout_bundle: None,
            sapling_bundle,
            orchard_bundle,
            #[cfg(feature = "zfuture")]
            tze_bundle,
        };

        //
        // Signatures -- everything but the signatures must already have been added.
        //
        let txid_parts = unauthed_tx.digest(TxIdDigester);

        let transparent_bundle = unauthed_tx.transparent_bundle.clone().map(|b| {
            b.apply_signatures(
                #[cfg(feature = "transparent-inputs")]
                &unauthed_tx,
                #[cfg(feature = "transparent-inputs")]
                &txid_parts,
            )
        });

        #[cfg(feature = "zfuture")]
        let tze_bundle = unauthed_tx
            .tze_bundle
            .clone()
            .map(|b| b.into_authorized(&unauthed_tx, tze_signers))
            .transpose()
            .map_err(Error::TzeBuild)?;

        // the commitment being signed is shared across all Sapling inputs; once
        // V4 transactions are deprecated this should just be the txid, but
        // for now we need to continue to compute it here.
        let shielded_sig_commitment =
            signature_hash(&unauthed_tx, &SignableInput::Shielded, &txid_parts);

        let (sapling_bundle, tx_metadata) = match unauthed_tx
            .sapling_bundle
            .map(|b| {
                b.apply_signatures(prover, &mut ctx, &mut rng, shielded_sig_commitment.as_ref())
            })
            .transpose()
            .map_err(Error::SaplingBuild)?
        {
            Some((bundle, meta)) => (Some(bundle), meta),
            None => (None, SaplingMetadata::empty()),
        };

        let orchard_saks = self.orchard_spending_keys.clone();

        let orchard_bundle = unauthed_tx
            .orchard_bundle
            .map(|b| {
                b.create_proof(&orchard::circuit::ProvingKey::build(), &mut rng)
                    .and_then(|b| {
                        b.apply_signatures(
                            &mut rng,
                            *shielded_sig_commitment.as_ref(),
                            &orchard_saks,
                        )
                    })
            })
            .transpose()
            .map_err(Error::OrchardBuild)?;

        let authorized_tx = TransactionData {
            version: unauthed_tx.version,
            consensus_branch_id: unauthed_tx.consensus_branch_id,
            lock_time: unauthed_tx.lock_time,
            expiry_height: unauthed_tx.expiry_height,
            transparent_bundle,
            sprout_bundle: unauthed_tx.sprout_bundle,
            sapling_bundle,
            orchard_bundle,
            #[cfg(feature = "zfuture")]
            tze_bundle,
        };

        // The unwrap() here is safe because the txid hashing
        // of freeze() should be infalliable.
        Ok((authorized_tx.freeze().unwrap(), tx_metadata))
    }
}

#[cfg(feature = "zfuture")]
impl<'a, P: consensus::Parameters, R: RngCore + CryptoRng> ExtensionTxBuilder<'a>
    for Builder<'a, P, R>
{
    type BuildCtx = TransactionData<Unauthorized>;
    type BuildError = tze::builder::Error;

    fn add_tze_input<WBuilder, W: ToPayload>(
        &mut self,
        extension_id: u32,
        mode: u32,
        prevout: (tze::OutPoint, TzeOut),
        witness_builder: WBuilder,
    ) -> Result<(), Self::BuildError>
    where
        WBuilder: 'a + (FnOnce(&Self::BuildCtx) -> Result<W, tze::builder::Error>),
    {
        self.tze_builder
            .add_input(extension_id, mode, prevout, witness_builder);

        Ok(())
    }

    fn add_tze_output<G: ToPayload>(
        &mut self,
        extension_id: u32,
        value: Amount,
        guarded_by: &G,
    ) -> Result<(), Self::BuildError> {
        self.tze_builder.add_output(extension_id, value, guarded_by)
    }
}

#[cfg(any(test, feature = "test-dependencies"))]
impl<'a, P: consensus::Parameters, R: RngCore> Builder<'a, P, R> {
    /// Creates a new `Builder` targeted for inclusion in the block with the given height
    /// and randomness source, using default values for general transaction fields.
    ///
    /// # Default values
    ///
    /// The expiry height will be set to the given height plus the default transaction
    /// expiry delta (20 blocks).
    ///
    /// The fee will be set to the default fee (0.0001 ZEC).
    ///
    /// WARNING: DO NOT USE IN PRODUCTION
    pub fn test_only_new_with_rng(
        params: P,
        height: BlockHeight,
        rng: R,
    ) -> Builder<'a, P, impl RngCore + CryptoRng> {
        struct FakeCryptoRng<R: RngCore>(R);
        impl<R: RngCore> CryptoRng for FakeCryptoRng<R> {}
        impl<R: RngCore> RngCore for FakeCryptoRng<R> {
            fn next_u32(&mut self) -> u32 {
                self.0.next_u32()
            }

            fn next_u64(&mut self) -> u64 {
                self.0.next_u64()
            }

            fn fill_bytes(&mut self, dest: &mut [u8]) {
                self.0.fill_bytes(dest)
            }

            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
                self.0.try_fill_bytes(dest)
            }
        }
        Builder::new_internal(params, height, FakeCryptoRng(rng), NoOrchardBuilder)
    }
}

#[cfg(test)]
mod tests {
    use ff::{Field, PrimeField};
    use rand_core::OsRng;

    use crate::{
        consensus::{NetworkUpgrade, Parameters, TEST_NETWORK},
        legacy::TransparentAddress,
        memo::MemoBytes,
        merkle_tree::{CommitmentTree, IncrementalWitness},
        sapling::{prover::mock::MockTxProver, Node, Rseed},
        transaction::components::{
            amount::{Amount, DEFAULT_FEE},
            orchard::builder::NoOrchardBuilder,
            sapling::builder::{self as build_s},
            transparent::builder::{self as build_t},
        },
        zip32::{ExtendedFullViewingKey, ExtendedSpendingKey},
    };

    use super::{Builder, Error, SaplingBuilder, DEFAULT_TX_EXPIRY_DELTA};

    #[cfg(feature = "zfuture")]
    use super::TzeBuilder;

    #[cfg(not(feature = "zfuture"))]
    use std::marker::PhantomData;

    #[test]
    fn fails_on_negative_output() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);
        let ovk = extfvk.fvk.ovk;
        let to = extfvk.default_address().1;

        let sapling_activation_height = TEST_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();

        let mut builder = Builder::new(TEST_NETWORK, sapling_activation_height);
        assert_eq!(
            builder.add_sapling_output(
                Some(ovk),
                to,
                Amount::from_i64(-1).unwrap(),
                MemoBytes::empty()
            ),
            Err(Error::SaplingBuild(build_s::Error::InvalidAmount))
        );
    }

    #[test]
    fn binding_sig_absent_if_no_shielded_spend_or_output() {
        use crate::consensus::NetworkUpgrade;
        use crate::transaction::builder::{self, TransparentBuilder};

        let sapling_activation_height = TEST_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();

        // Create a builder with 0 fee, so we can construct t outputs
        let mut builder = builder::Builder {
            params: TEST_NETWORK,
            rng: OsRng,
            target_height: sapling_activation_height,
            expiry_height: sapling_activation_height + DEFAULT_TX_EXPIRY_DELTA,
            fee: Amount::zero(),
            transparent_builder: TransparentBuilder::empty(),
            sapling_builder: SaplingBuilder::new(TEST_NETWORK, sapling_activation_height),
            contains_orchard: false,
            orchard_builder: NoOrchardBuilder,
            orchard_spending_keys: Vec::new(),
            change_address: None,
            #[cfg(feature = "zfuture")]
            tze_builder: TzeBuilder::empty(),
            #[cfg(not(feature = "zfuture"))]
            tze_builder: PhantomData,
            progress_notifier: None,
        };

        // Create a tx with only t output. No binding_sig should be present
        builder
            .add_transparent_output(&TransparentAddress::PublicKey([0; 20]), Amount::zero())
            .unwrap();

        let (tx, _) = builder.build(&MockTxProver).unwrap();
        // No binding signature, because only t input and outputs
        assert!(tx.sapling_bundle.is_none());
    }

    #[test]
    fn binding_sig_present_if_shielded_spend() {
        let extsk = ExtendedSpendingKey::master(&[]);
        let extfvk = ExtendedFullViewingKey::from(&extsk);
        let to = extfvk.default_address().1;

        let mut rng = OsRng;

        let note1 = to
            .create_note(50000, Rseed::BeforeZip212(jubjub::Fr::random(&mut rng)))
            .unwrap();
        let cmu1 = Node::new(note1.cmu().to_repr());
        let mut tree = CommitmentTree::empty();
        tree.append(cmu1).unwrap();
        let witness1 = IncrementalWitness::from_tree(&tree);

        let tx_height = TEST_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();
        let mut builder = Builder::new(TEST_NETWORK, tx_height);

        // Create a tx with a sapling spend. binding_sig should be present
        builder
            .add_sapling_spend(extsk, *to.diversifier(), note1, witness1.path().unwrap())
            .unwrap();

        builder
            .add_transparent_output(&TransparentAddress::PublicKey([0; 20]), Amount::zero())
            .unwrap();

        // Expect a binding signature error, because our inputs aren't valid, but this shows
        // that a binding signature was attempted
        assert_eq!(
            builder.build(&MockTxProver),
            Err(Error::SaplingBuild(build_s::Error::BindingSig))
        );
    }

    #[test]
    fn fails_on_negative_transparent_output() {
        let tx_height = TEST_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();
        let mut builder = Builder::new(TEST_NETWORK, tx_height);
        assert_eq!(
            builder.add_transparent_output(
                &TransparentAddress::PublicKey([0; 20]),
                Amount::from_i64(-1).unwrap(),
            ),
            Err(Error::TransparentBuild(build_t::Error::InvalidAmount))
        );
    }

    #[test]
    fn fails_on_negative_change() {
        let mut rng = OsRng;

        // Just use the master key as the ExtendedSpendingKey for this test
        let extsk = ExtendedSpendingKey::master(&[]);
        let tx_height = TEST_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();

        // Fails with no inputs or outputs
        // 0.0001 t-ZEC fee
        {
            let builder = Builder::new(TEST_NETWORK, tx_height);
            assert_eq!(
                builder.build(&MockTxProver),
                Err(Error::ChangeIsNegative(
                    (Amount::zero() - DEFAULT_FEE).unwrap()
                ))
            );
        }

        let extfvk = ExtendedFullViewingKey::from(&extsk);
        let ovk = Some(extfvk.fvk.ovk);
        let to = extfvk.default_address().1;

        // Fail if there is only a Sapling output
        // 0.0005 z-ZEC out, 0.00001 t-ZEC fee
        {
            let mut builder = Builder::new(TEST_NETWORK, tx_height);
            builder
                .add_sapling_output(
                    ovk,
                    to.clone(),
                    Amount::from_u64(50000).unwrap(),
                    MemoBytes::empty(),
                )
                .unwrap();
            assert_eq!(
                builder.build(&MockTxProver),
                Err(Error::ChangeIsNegative(
                    (Amount::from_i64(-50000).unwrap() - DEFAULT_FEE).unwrap()
                ))
            );
        }

        // Fail if there is only a transparent output
        // 0.0005 t-ZEC out, 0.00001 t-ZEC fee
        {
            let mut builder = Builder::new(TEST_NETWORK, tx_height);
            builder
                .add_transparent_output(
                    &TransparentAddress::PublicKey([0; 20]),
                    Amount::from_u64(50000).unwrap(),
                )
                .unwrap();
            assert_eq!(
                builder.build(&MockTxProver),
                Err(Error::ChangeIsNegative(
                    (Amount::from_i64(-50000).unwrap() - DEFAULT_FEE).unwrap()
                ))
            );
        }

        let note1 = to
            .create_note(50999, Rseed::BeforeZip212(jubjub::Fr::random(&mut rng)))
            .unwrap();
        let cmu1 = Node::new(note1.cmu().to_repr());
        let mut tree = CommitmentTree::empty();
        tree.append(cmu1).unwrap();
        let mut witness1 = IncrementalWitness::from_tree(&tree);

        // Fail if there is insufficient input
        // 0.0003 z-ZEC out, 0.0002 t-ZEC out, 0.00001 t-ZEC fee, 0.00050999 z-ZEC in
        {
            let mut builder = Builder::new(TEST_NETWORK, tx_height);
            builder
                .add_sapling_spend(
                    extsk.clone(),
                    *to.diversifier(),
                    note1.clone(),
                    witness1.path().unwrap(),
                )
                .unwrap();
            builder
                .add_sapling_output(
                    ovk,
                    to.clone(),
                    Amount::from_u64(30000).unwrap(),
                    MemoBytes::empty(),
                )
                .unwrap();
            builder
                .add_transparent_output(
                    &TransparentAddress::PublicKey([0; 20]),
                    Amount::from_u64(20000).unwrap(),
                )
                .unwrap();
            assert_eq!(
                builder.build(&MockTxProver),
                Err(Error::ChangeIsNegative(Amount::from_i64(-1).unwrap()))
            );
        }

        let note2 = to
            .create_note(1, Rseed::BeforeZip212(jubjub::Fr::random(&mut rng)))
            .unwrap();
        let cmu2 = Node::new(note2.cmu().to_repr());
        tree.append(cmu2).unwrap();
        witness1.append(cmu2).unwrap();
        let witness2 = IncrementalWitness::from_tree(&tree);

        // Succeeds if there is sufficient input
        // 0.0003 z-ZEC out, 0.0002 t-ZEC out, 0.0001 t-ZEC fee, 0.0006 z-ZEC in
        //
        // (Still fails because we are using a MockTxProver which doesn't correctly
        // compute bindingSig.)
        {
            let mut builder = Builder::new(TEST_NETWORK, tx_height);
            builder
                .add_sapling_spend(
                    extsk.clone(),
                    *to.diversifier(),
                    note1,
                    witness1.path().unwrap(),
                )
                .unwrap();
            builder
                .add_sapling_spend(extsk, *to.diversifier(), note2, witness2.path().unwrap())
                .unwrap();
            builder
                .add_sapling_output(
                    ovk,
                    to,
                    Amount::from_u64(30000).unwrap(),
                    MemoBytes::empty(),
                )
                .unwrap();
            builder
                .add_transparent_output(
                    &TransparentAddress::PublicKey([0; 20]),
                    Amount::from_u64(20000).unwrap(),
                )
                .unwrap();
            assert_eq!(
                builder.build(&MockTxProver),
                Err(Error::SaplingBuild(build_s::Error::BindingSig))
            )
        }
    }
}
