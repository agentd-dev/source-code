// SPDX-License-Identifier: AGPL-3.0-only
//! The `exact` scheme on Solana (RFC 0049 §10; [EXACT-SVM]): the transfer an
//! `accepts` entry on a `solana:` network asks for, the transaction that
//! makes it, signed with the wallet's Ed25519 key, and the `PaymentPayload`
//! it travels in.
//!
//! **What the entry asks** ([`Terms`]): `asset` is the token's mint, `payTo`
//! the merchant's key, `amount` atomic units, and `extra.feePayer` the key of
//! the sponsor that pays the network fee and signs last — agentd never pays
//! a fee. `extra.memo`, when the server sets one, is the memo the
//! transaction carries; `extra.recentBlockhash`, when the server sets one
//! that reads, is the blockhash it is built on — else agentd asks the RPC
//! endpoint the operator names for the network (`getLatestBlockhash`).
//!
//! **The transaction** is the one x402's reference client builds, and
//! nothing else: a version-0 message whose fee payer is `extra.feePayer`,
//! with four instructions in order — the compute-budget program's
//! `SetComputeUnitLimit` (20 000) and `SetComputeUnitPrice` (1 microlamport),
//! the token program's `TransferChecked` of `amount` at the asset's
//! `decimals`, from the wallet's associated token account to `payTo`'s, its
//! authority the wallet; and a Memo of `extra.memo` or of 16 random bytes in
//! hex. The fee payer appears in no instruction, so its signature can spend
//! nothing of its own [EXACT-SVM §2.1.1]. Accounts are ordered as
//! `@solana/web3.js` compiles them; no address lookup table is used. agentd
//! signs the message as the second signer; the first signature, the fee
//! payer's, is left zero for the sponsor to fill when it settles.
//!
//! **Associated token accounts** are program-derived addresses: SHA-256 of
//! the owner, the token program and the mint, a bump seed, the associated
//! token program and `"ProgramDerivedAddress"`, the first bump from 255 down
//! whose hash is not a point of Ed25519 ([`super::ed25519`]).
//!
//! [`Transfer::read`] reads a message back, strictly: exactly that shape, or
//! an error naming what differs. The external signer's policy, the mock
//! server's verifier and the tests all hold a message to it.

use std::time::Duration;

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};

use super::base58;
use super::caip::AssetId;
use super::x402::Entry;

/// The SPL Token program.
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// The Token-2022 program.
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
/// The associated token account program.
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
/// The compute-budget program.
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
/// The SPL Memo program.
pub const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// The compute units the transaction asks for: the reference client's.
pub const COMPUTE_UNIT_LIMIT: u32 = 20_000;
/// The priority fee it offers, in microlamports per compute unit: the
/// reference client's. The sponsor pays it.
pub const COMPUTE_UNIT_PRICE: u64 = 1;
/// The most a sponsor accepts on x402's fast path: 5 lamports per unit.
pub const MAX_COMPUTE_UNIT_PRICE: u64 = 5_000_000;
/// The longest memo: 256 bytes [EXACT-SVM].
pub const MAX_MEMO: usize = 256;
/// The random memo's bytes, before hex: 16 [EXACT-SVM §3.1].
pub const MEMO_NONCE: usize = 16;
/// How long a signed transfer is counted as able to land, in seconds. A
/// blockhash expires 150 blocks after it, about 60 to 90 seconds at
/// Solana's 400 ms slot; agentd reads no block height, so it counts five
/// times the 60 seconds.
pub const LANDS_WITHIN_S: u64 = 300;
/// The bound on a `getLatestBlockhash` answer.
pub const MAX_RPC_ANSWER: usize = 64 * 1024;
/// How long the RPC endpoint is waited for.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

const SET_COMPUTE_UNIT_LIMIT: u8 = 2;
const SET_COMPUTE_UNIT_PRICE: u8 = 3;
const TRANSFER_CHECKED: u8 = 12;
/// A version-0 message's prefix: the high bit, and version 0.
const V0: u8 = 0x80;

/// A Solana public key: an account, a mint or a program.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pubkey(pub [u8; 32]);

impl Pubkey {
    /// 32 bytes in base58.
    pub fn parse(s: &str) -> Result<Pubkey, String> {
        base58::decode32(s)
            .map(Pubkey)
            .map_err(|e| format!("{s:?} is not a Solana address (32 bytes in base58): {e}"))
    }

    /// A program id this module names.
    fn known(s: &str) -> Pubkey {
        Pubkey(base58::decode32(s).unwrap_or([0; 32]))
    }

    /// Whether these bytes are a point of Ed25519: a key someone may hold.
    /// A program-derived address is not.
    pub fn on_curve(&self) -> bool {
        super::ed25519::is_on_curve(&self.0)
    }
}

impl std::fmt::Display for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&base58::encode(&self.0))
    }
}

impl std::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

/// The program a token's mint belongs to: its transfers and its associated
/// token accounts are that program's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProgram {
    /// SPL Token (`spl-token`), the program USDC and USDT on Solana use.
    Spl,
    /// Token-2022 (`token-2022`).
    Token2022,
}

impl TokenProgram {
    /// As `security.payments.assets.<name>.token_program` names it.
    pub fn parse(s: &str) -> Result<TokenProgram, String> {
        match s {
            "spl-token" => Ok(TokenProgram::Spl),
            "token-2022" => Ok(TokenProgram::Token2022),
            _ => Err(format!(
                "{s:?} is not a token program (spl-token or token-2022)"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TokenProgram::Spl => "spl-token",
            TokenProgram::Token2022 => "token-2022",
        }
    }

    pub fn id(self) -> Pubkey {
        Pubkey::known(match self {
            TokenProgram::Spl => TOKEN_PROGRAM,
            TokenProgram::Token2022 => TOKEN_2022_PROGRAM,
        })
    }

    /// The program whose id this is.
    pub fn of(id: &Pubkey) -> Option<TokenProgram> {
        [TokenProgram::Spl, TokenProgram::Token2022]
            .into_iter()
            .find(|p| p.id() == *id)
    }
}

/// The mint of an SPL token on a Solana network, from its CAIP-19 id
/// (`solana:<genesis hash prefix>/token:<mint>`).
pub fn mint_of(id: &AssetId) -> Option<Pubkey> {
    if id.chain.namespace != "solana" || id.namespace != "token" {
        return None;
    }
    Pubkey::parse(&id.reference).ok()
}

/// The address `seeds` derive under `program`, when it is not a point of
/// the curve: SHA-256 of the seeds, the program and
/// `"ProgramDerivedAddress"`. At most 16 seeds of at most 32 bytes.
pub fn create_program_address(seeds: &[&[u8]], program: &Pubkey) -> Option<Pubkey> {
    if seeds.len() > 16 || seeds.iter().any(|s| s.len() > 32) {
        return None;
    }
    let mut buf = Vec::with_capacity(seeds.iter().map(|s| s.len()).sum::<usize>() + 64);
    for s in seeds {
        buf.extend_from_slice(s);
    }
    buf.extend_from_slice(&program.0);
    buf.extend_from_slice(b"ProgramDerivedAddress");
    let address = Pubkey(crate::sha::sha256(&buf));
    (!address.on_curve()).then_some(address)
}

/// The program-derived address of `seeds` and its bump: the first bump
/// from 255 down to 1 that gives an address off the curve.
pub fn find_program_address(seeds: &[&[u8]], program: &Pubkey) -> Option<(Pubkey, u8)> {
    (1..=255u8).rev().find_map(|bump| {
        let b = [bump];
        let mut with: Vec<&[u8]> = seeds.to_vec();
        with.push(&b);
        create_program_address(&with, program).map(|a| (a, bump))
    })
}

/// The associated token account of `owner` for `mint`, under `program`.
pub fn associated_token_account(owner: &Pubkey, mint: &Pubkey, program: TokenProgram) -> Pubkey {
    let seeds: [&[u8]; 3] = [&owner.0, &program.id().0, &mint.0];
    // Some bump of 255 gives an address off the curve unless SHA-256 is
    // broken: each is a point with probability about one half.
    find_program_address(&seeds, &Pubkey::known(ASSOCIATED_TOKEN_PROGRAM))
        .map_or(Pubkey([0; 32]), |(a, _)| a)
}

/// `n` in Solana's compact-u16: seven bits a byte, least significant first,
/// the high bit set while more follow.
pub fn compact_u16(n: u16, out: &mut Vec<u8>) {
    let mut rest = n;
    loop {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        if rest == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A compact-u16 at `bytes[*at..]`, read in its shortest form only: a
/// value written longer than it needs is not one, as the runtime holds it.
pub fn read_compact_u16(bytes: &[u8], at: &mut usize) -> Result<u16, String> {
    let mut v: u32 = 0;
    for i in 0..3 {
        let b = *bytes.get(*at).ok_or("ends inside a length")?;
        *at += 1;
        v |= u32::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err("a length not in its shortest form".into());
            }
            return u16::try_from(v).map_err(|_| "a length past 16 bits".to_string());
        }
    }
    Err("a length past 16 bits".into())
}

/// An account an instruction names, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub key: Pubkey,
    pub signer: bool,
    pub writable: bool,
}

/// An instruction before it is compiled into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub program: Pubkey,
    pub accounts: Vec<Meta>,
    pub data: Vec<u8>,
}

/// The compute-budget program's `SetComputeUnitLimit`.
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![SET_COMPUTE_UNIT_LIMIT];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program: Pubkey::known(COMPUTE_BUDGET_PROGRAM),
        accounts: Vec::new(),
        data,
    }
}

/// The compute-budget program's `SetComputeUnitPrice`, in microlamports.
pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![SET_COMPUTE_UNIT_PRICE];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program: Pubkey::known(COMPUTE_BUDGET_PROGRAM),
        accounts: Vec::new(),
        data,
    }
}

/// A token program's `TransferChecked`: `amount` at `decimals` of `mint`
/// from `source` to `destination`, signed by `authority`.
pub fn transfer_checked(
    program: TokenProgram,
    source: Pubkey,
    mint: Pubkey,
    destination: Pubkey,
    authority: Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = vec![TRANSFER_CHECKED];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let meta = |key, signer, writable| Meta {
        key,
        signer,
        writable,
    };
    Instruction {
        program: program.id(),
        accounts: vec![
            meta(source, false, true),
            meta(mint, false, false),
            meta(destination, false, true),
            meta(authority, true, false),
        ],
        data,
    }
}

/// The Memo program's instruction: `text`, no accounts.
pub fn memo(text: &[u8]) -> Instruction {
    Instruction {
        program: Pubkey::known(MEMO_PROGRAM),
        accounts: Vec::new(),
        data: text.to_vec(),
    }
}

/// An instruction in a message: indexes into its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub program: u8,
    pub accounts: Vec<u8>,
    pub data: Vec<u8>,
}

/// A version-0 message with no address lookup table: what a transaction's
/// signatures sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// How many keys, first, sign.
    pub signers: u8,
    /// How many of those are read-only.
    pub readonly_signed: u8,
    /// How many keys, last, are read-only and sign nothing.
    pub readonly_unsigned: u8,
    pub keys: Vec<Pubkey>,
    pub blockhash: [u8; 32],
    pub instructions: Vec<Compiled>,
}

impl Message {
    /// `instructions` compiled with `payer` as the fee payer: every key once,
    /// the writable signers first (the payer first of them), then the
    /// read-only signers, the writable keys and the read-only ones, each
    /// group in the order its keys first appear — as `@solana/web3.js`
    /// compiles a message.
    pub fn compile(
        payer: Pubkey,
        instructions: &[Instruction],
        blockhash: [u8; 32],
    ) -> Result<Message, String> {
        // (key, signer, writable), in the order first seen.
        let mut seen: Vec<(Pubkey, bool, bool)> = vec![(payer, true, true)];
        let mut note = |key: Pubkey, signer: bool, writable: bool| match seen
            .iter_mut()
            .find(|(k, _, _)| *k == key)
        {
            Some(m) => {
                m.1 |= signer;
                m.2 |= writable;
            }
            None => seen.push((key, signer, writable)),
        };
        for ix in instructions {
            note(ix.program, false, false);
            for m in &ix.accounts {
                note(m.key, m.signer, m.writable);
            }
        }
        let group = |signer: bool, writable: bool| -> Vec<Pubkey> {
            seen.iter()
                .filter(|(_, s, w)| *s == signer && *w == writable)
                .map(|(k, _, _)| *k)
                .collect()
        };
        let (ws, rs, wu, ru) = (
            group(true, true),
            group(true, false),
            group(false, true),
            group(false, false),
        );
        let count = |n: usize| u8::try_from(n).map_err(|_| "more than 255 keys".to_string());
        let header = (
            count(ws.len() + rs.len())?,
            count(rs.len())?,
            count(ru.len())?,
        );
        let keys: Vec<Pubkey> = ws.into_iter().chain(rs).chain(wu).chain(ru).collect();
        count(keys.len())?;
        let index = |k: &Pubkey| {
            keys.iter()
                .position(|x| x == k)
                .map(|i| i as u8)
                .unwrap_or(0)
        };
        let instructions = instructions
            .iter()
            .map(|ix| Compiled {
                program: index(&ix.program),
                accounts: ix.accounts.iter().map(|m| index(&m.key)).collect(),
                data: ix.data.clone(),
            })
            .collect();
        Ok(Message {
            signers: header.0,
            readonly_signed: header.1,
            readonly_unsigned: header.2,
            keys,
            blockhash,
            instructions,
        })
    }

    /// The bytes a signature signs: the version prefix, the header, the
    /// keys, the blockhash, the instructions, and an empty list of lookup
    /// tables, each list behind its compact-u16 length.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![
            V0,
            self.signers,
            self.readonly_signed,
            self.readonly_unsigned,
        ];
        compact_u16(self.keys.len() as u16, &mut out);
        for k in &self.keys {
            out.extend_from_slice(&k.0);
        }
        out.extend_from_slice(&self.blockhash);
        compact_u16(self.instructions.len() as u16, &mut out);
        for ix in &self.instructions {
            out.push(ix.program);
            compact_u16(ix.accounts.len() as u16, &mut out);
            out.extend_from_slice(&ix.accounts);
            compact_u16(ix.data.len() as u16, &mut out);
            out.extend_from_slice(&ix.data);
        }
        compact_u16(0, &mut out);
        out
    }

    /// A message's bytes: version 0, no address lookup table, every index
    /// within its keys, nothing after it.
    pub fn parse(bytes: &[u8]) -> Result<Message, String> {
        let mut at = 0;
        let byte = |at: &mut usize| -> Result<u8, String> {
            let b = *bytes.get(*at).ok_or("the message ends early")?;
            *at += 1;
            Ok(b)
        };
        let prefix = byte(&mut at)?;
        if prefix != V0 {
            return Err(if prefix & 0x80 == 0 {
                "a legacy message: x402's exact scheme on Solana sends a version-0 one".into()
            } else {
                format!(
                    "a version-{} message: there is only version 0",
                    prefix & 0x7f
                )
            });
        }
        let (signers, readonly_signed, readonly_unsigned) =
            (byte(&mut at)?, byte(&mut at)?, byte(&mut at)?);
        let n = usize::from(read_compact_u16(bytes, &mut at)?);
        let take = |at: &mut usize, len: usize| -> Result<&[u8], String> {
            let s = bytes.get(*at..*at + len).ok_or("the message ends early")?;
            *at += len;
            Ok(s)
        };
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(Pubkey(take(&mut at, 32)?.try_into().map_err(|_| "a key")?));
        }
        if signers == 0
            || usize::from(signers) > n
            || readonly_signed >= signers
            || usize::from(signers) + usize::from(readonly_unsigned) > n
        {
            return Err("a header that does not fit its keys".into());
        }
        let blockhash: [u8; 32] = take(&mut at, 32)?.try_into().map_err(|_| "the blockhash")?;
        let count = read_compact_u16(bytes, &mut at)?;
        let mut instructions = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let program = byte(&mut at)?;
            let k = usize::from(read_compact_u16(bytes, &mut at)?);
            let accounts = take(&mut at, k)?.to_vec();
            let d = usize::from(read_compact_u16(bytes, &mut at)?);
            let data = take(&mut at, d)?.to_vec();
            if usize::from(program) >= n || accounts.iter().any(|a| usize::from(*a) >= n) {
                return Err("an instruction names a key the message does not have".into());
            }
            instructions.push(Compiled {
                program,
                accounts,
                data,
            });
        }
        if read_compact_u16(bytes, &mut at)? != 0 {
            return Err("address lookup tables: agentd builds none and reads none".into());
        }
        if at != bytes.len() {
            return Err("bytes after the message".into());
        }
        Ok(Message {
            signers,
            readonly_signed,
            readonly_unsigned,
            keys,
            blockhash,
            instructions,
        })
    }

    pub fn is_signer(&self, i: usize) -> bool {
        i < usize::from(self.signers)
    }

    pub fn is_writable(&self, i: usize) -> bool {
        let (s, n) = (usize::from(self.signers), self.keys.len());
        if i < s {
            i < s - usize::from(self.readonly_signed)
        } else {
            i < n - usize::from(self.readonly_unsigned)
        }
    }
}

/// A transaction's bytes: one 64-byte signature per signer, in key order,
/// then the message.
pub fn transaction(signatures: &[[u8; 64]], message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 64 * signatures.len() + message.len());
    compact_u16(signatures.len() as u16, &mut out);
    for s in signatures {
        out.extend_from_slice(s);
    }
    out.extend_from_slice(message);
    out
}

/// A transaction, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// One per signer, in key order; all zero where not signed yet.
    pub signatures: Vec<[u8; 64]>,
    pub message: Message,
    /// The message's bytes: what each signature signs.
    pub signed: Vec<u8>,
}

impl Transaction {
    /// A transaction's bytes: as many signatures as its message has
    /// signers, then the message.
    pub fn read(bytes: &[u8]) -> Result<Transaction, String> {
        let mut at = 0;
        let n = usize::from(read_compact_u16(bytes, &mut at)?);
        let mut signatures = Vec::with_capacity(n);
        for _ in 0..n {
            let s = bytes
                .get(at..at + 64)
                .ok_or("the transaction ends inside a signature")?;
            signatures.push(<[u8; 64]>::try_from(s).map_err(|_| "a signature")?);
            at += 64;
        }
        let signed = bytes[at..].to_vec();
        let message = Message::parse(&signed)?;
        if usize::from(message.signers) != n {
            return Err(format!(
                "{n} signatures for a message of {} signers",
                message.signers
            ));
        }
        Ok(Transaction {
            signatures,
            message,
            signed,
        })
    }
}

/// The transfer a message makes, read strictly: x402's exact transaction as
/// agentd builds it, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub fee_payer: Pubkey,
    /// The wallet: the transfer's authority, the second signer.
    pub owner: Pubkey,
    pub program: TokenProgram,
    pub mint: Pubkey,
    pub source: Pubkey,
    pub destination: Pubkey,
    pub amount: u64,
    pub decimals: u8,
    pub memo: Vec<u8>,
    pub unit_limit: u32,
    pub unit_price: u64,
    pub blockhash: [u8; 32],
}

impl Transfer {
    /// The transfer `m` makes, when it is exactly: two signers, the fee
    /// payer writable and the owner read-only; the compute-unit limit and
    /// price, a `TransferChecked` and a Memo of at most 256 bytes of UTF-8,
    /// in that order and no other; the fee payer in no instruction; the
    /// source the owner's associated token account for the mint; eight
    /// keys, each one role. `Err` names what differs.
    pub fn read(m: &Message) -> Result<Transfer, String> {
        if m.signers != 2 || m.readonly_signed != 1 {
            return Err("not two signers, the fee payer and the wallet".into());
        }
        if m.keys.len() != 8 {
            return Err(format!(
                "{} keys, not the eight of a transfer (fee payer, wallet, two token accounts, mint and three programs)",
                m.keys.len()
            ));
        }
        if m.instructions.len() != 4 {
            return Err(format!(
                "{} instructions, not the four of x402's exact transfer",
                m.instructions.len()
            ));
        }
        for ix in &m.instructions {
            let p = usize::from(ix.program);
            if m.is_signer(p) || m.is_writable(p) {
                return Err("a program that signs or is written".into());
            }
            if ix.accounts.contains(&0) {
                return Err(
                    "the fee payer is in an instruction: its signature could spend its own".into(),
                );
            }
        }
        let program = |i: usize| m.keys[usize::from(m.instructions[i].program)];
        let data = |i: usize| m.instructions[i].data.as_slice();
        let no_accounts = |i: usize| m.instructions[i].accounts.is_empty();
        let budget = Pubkey::known(COMPUTE_BUDGET_PROGRAM);
        let unit_limit = match data(0) {
            [SET_COMPUTE_UNIT_LIMIT, rest @ ..]
                if program(0) == budget && no_accounts(0) && rest.len() == 4 =>
            {
                u32::from_le_bytes(rest.try_into().map_err(|_| "units")?)
            }
            _ => return Err("the first instruction is not SetComputeUnitLimit".into()),
        };
        let unit_price = match data(1) {
            [SET_COMPUTE_UNIT_PRICE, rest @ ..]
                if program(1) == budget && no_accounts(1) && rest.len() == 8 =>
            {
                u64::from_le_bytes(rest.try_into().map_err(|_| "price")?)
            }
            _ => return Err("the second instruction is not SetComputeUnitPrice".into()),
        };
        let token = TokenProgram::of(&program(2))
            .ok_or("the third instruction is not the SPL Token or Token-2022 program's")?;
        let (amount, decimals) = match data(2) {
            [TRANSFER_CHECKED, rest @ ..] if rest.len() == 9 => (
                u64::from_le_bytes(rest[..8].try_into().map_err(|_| "amount")?),
                rest[8],
            ),
            _ => return Err("the third instruction is not TransferChecked".into()),
        };
        let accounts = &m.instructions[2].accounts;
        if accounts.len() != 4 {
            return Err("TransferChecked names other than its four accounts".into());
        }
        let [source, mint, destination, authority] =
            [accounts[0], accounts[1], accounts[2], accounts[3]].map(usize::from);
        if authority != 1 {
            return Err("the transfer's authority is not the wallet, the second signer".into());
        }
        if !m.is_writable(source) || !m.is_writable(destination) || m.is_writable(mint) {
            return Err("the token accounts are not writable, or the mint is".into());
        }
        if [source, mint, destination].iter().any(|i| m.is_signer(*i)) {
            return Err("a token account or the mint signs".into());
        }
        if program(3) != Pubkey::known(MEMO_PROGRAM) || !no_accounts(3) {
            return Err("the fourth instruction is not a Memo".into());
        }
        let memo = data(3);
        if memo.len() > MAX_MEMO || std::str::from_utf8(memo).is_err() {
            return Err("the memo is not at most 256 bytes of UTF-8".into());
        }
        let t = Transfer {
            fee_payer: m.keys[0],
            owner: m.keys[1],
            program: token,
            mint: m.keys[mint],
            source: m.keys[source],
            destination: m.keys[destination],
            amount,
            decimals,
            memo: memo.to_vec(),
            unit_limit,
            unit_price,
            blockhash: m.blockhash,
        };
        let mut roles = vec![
            t.fee_payer,
            t.owner,
            t.source,
            t.destination,
            t.mint,
            budget,
            token.id(),
            Pubkey::known(MEMO_PROGRAM),
        ];
        roles.sort_unstable();
        roles.dedup();
        if roles.len() != 8 {
            return Err("one key in two roles".into());
        }
        if t.source != associated_token_account(&t.owner, &t.mint, token) {
            return Err(
                "the source is not the wallet's associated token account for the mint".into(),
            );
        }
        Ok(t)
    }

    /// Whether the transfer pays `pay_to`: its destination is `pay_to`'s
    /// associated token account for the mint.
    pub fn pays(&self, pay_to: &Pubkey) -> bool {
        self.destination == associated_token_account(pay_to, &self.mint, self.program)
    }
}

/// What an `accepts` entry on Solana asks beyond the amount, read with the
/// asset it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms {
    pub mint: Pubkey,
    pub program: TokenProgram,
    pub decimals: u8,
    pub pay_to: Pubkey,
    /// `extra.feePayer`: the sponsor that pays the fee and signs last.
    pub fee_payer: Pubkey,
    /// `extra.memo`.
    pub memo: Option<String>,
    /// `extra.recentBlockhash`, when it reads as one.
    pub blockhash: Option<[u8; 32]>,
    /// The RPC endpoint the operator names for the network, asked for a
    /// blockhash when the server gives none.
    pub rpc: Option<String>,
}

impl Terms {
    /// `e`'s Solana terms, paying `pay_to` in `mint` (of `program`, at
    /// `decimals`), with `rpc` the network's endpoint: every reason they
    /// are not pushed on `why`. `pay_to` is `None` when the caller could not
    /// read it, and has said why.
    pub fn read(
        e: &Entry,
        mint: Pubkey,
        program: TokenProgram,
        decimals: u8,
        pay_to: Option<Pubkey>,
        rpc: Option<&str>,
        why: &mut Vec<String>,
    ) -> Option<Terms> {
        let n = why.len();
        let fee_payer = match e.extra.get("feePayer").and_then(Value::as_str) {
            None => {
                why.push("extra: no feePayer, the sponsor who pays the network fee".into());
                None
            }
            Some(f) => Pubkey::parse(f)
                .map_err(|_| why.push(format!("extra.feePayer {f:?}: not a Solana address")))
                .ok(),
        };
        let memo = match e.extra.get("memo") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.len() <= MAX_MEMO => Some(s.clone()),
            Some(Value::String(_)) => {
                why.push(format!("extra.memo: more than {MAX_MEMO} bytes"));
                None
            }
            Some(_) => {
                why.push("extra.memo: not a string".into());
                None
            }
        };
        // A blockhash that does not read is as none: the client fetches its
        // own [EXACT-SVM].
        let blockhash = e
            .extra
            .get("recentBlockhash")
            .and_then(Value::as_str)
            .and_then(|b| base58::decode32(b).ok());
        if blockhash.is_none() && rpc.is_none() {
            why.push(format!(
                "extra: no recentBlockhash, and security.payments.rpc names no endpoint for {} to ask for one",
                e.network
            ));
        }
        if why.len() != n {
            return None;
        }
        Some(Terms {
            mint,
            program,
            decimals,
            pay_to: pay_to?,
            fee_payer: fee_payer?,
            memo,
            blockhash,
            rpc: rpc.map(str::to_string),
        })
    }

    /// The memo the transaction carries: `extra.memo`, or `nonce`'s first
    /// [`MEMO_NONCE`] bytes in hex.
    pub fn memo_for(&self, nonce: &[u8; 32]) -> String {
        match &self.memo {
            Some(m) => m.clone(),
            None => crate::sha::to_hex(&nonce[..MEMO_NONCE]),
        }
    }

    /// The message that pays `amount` from `owner` with the memo `text`,
    /// built on `blockhash`. Refused when the fee payer is the wallet itself
    /// — agentd pays no network fee — or a key of the transfer.
    pub fn message(
        &self,
        owner: &Pubkey,
        amount: u64,
        text: &[u8],
        blockhash: [u8; 32],
    ) -> Result<Message, String> {
        if self.fee_payer == *owner {
            return Err("extra.feePayer is the wallet itself: agentd pays no network fee".into());
        }
        let source = associated_token_account(owner, &self.mint, self.program);
        let destination = associated_token_account(&self.pay_to, &self.mint, self.program);
        let ixs = [
            set_compute_unit_limit(COMPUTE_UNIT_LIMIT),
            set_compute_unit_price(COMPUTE_UNIT_PRICE),
            transfer_checked(
                self.program,
                source,
                self.mint,
                destination,
                *owner,
                amount,
                self.decimals,
            ),
            memo(text),
        ];
        let m = Message::compile(self.fee_payer, &ixs, blockhash)?;
        // What agentd signs is what it reads back, or nothing.
        Transfer::read(&m).map_err(|e| {
            format!("the transaction for this entry is not x402's exact transfer: {e}")
        })?;
        Ok(m)
    }
}

/// The key of a 32-byte Ed25519 seed (RFC 8032).
pub fn key_pair(seed: &[u8; 32]) -> Result<Ed25519KeyPair, String> {
    Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| "not an Ed25519 seed".to_string())
}

/// The public key of an Ed25519 key pair: its Solana address.
pub fn pubkey_of(kp: &Ed25519KeyPair) -> Pubkey {
    let mut k = [0u8; 32];
    k.copy_from_slice(kp.public_key().as_ref());
    Pubkey(k)
}

/// The 64-byte Ed25519 signature of `message` by `kp`.
pub fn sign(kp: &Ed25519KeyPair, message: &[u8]) -> [u8; 64] {
    let mut s = [0u8; 64];
    s.copy_from_slice(kp.sign(message).as_ref());
    s
}

/// Whether `sig` is `key`'s Ed25519 signature of `message`.
pub fn verify(key: &Pubkey, message: &[u8], sig: &[u8; 64]) -> bool {
    UnparsedPublicKey::new(&ED25519, key.0)
        .verify(message, sig)
        .is_ok()
}

/// An Ed25519 public key as a JWK (RFC 8037): `kty: OKP`, `crv: Ed25519`.
pub fn jwk_of(key: &Pubkey) -> Value {
    json!({"kty": "OKP", "crv": "Ed25519", "x": crate::b64::url_nopad(&key.0)})
}

/// The Solana address of an Ed25519 public JWK, as the external signer
/// describes a key; a JWK with a private member or of another curve is
/// refused.
pub fn pubkey_of_jwk(jwk: &Value) -> Result<Pubkey, String> {
    if jwk.get("d").is_some() {
        return Err("the JWK carries a private member".into());
    }
    if jwk["kty"] != "OKP" || jwk["crv"] != "Ed25519" {
        return Err("not an Ed25519 JWK (kty OKP, crv Ed25519)".into());
    }
    jwk["x"]
        .as_str()
        .and_then(|s| crate::b64::url_decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .map(Pubkey)
        .ok_or_else(|| "the JWK's x is not 32 base64url bytes".to_string())
}

/// The `payload` of the `PaymentPayload` that pays `amount` on Solana from
/// `wallet` under `terms`, with `memo` (the one the ledger's record names)
/// [EXACT-SVM]: the blockhash the entry gave or, when it gave none, the RPC
/// endpoint's; the message built, held to [`Transfer::read`], and signed by
/// the wallet — the signature verified under its address — and the
/// transaction partially signed, the fee payer's slot left zero, in
/// standard base64. Nothing is sent; an error means nothing was signed for
/// the server.
pub fn signed_transfer(
    wallets: &super::wallet::Wallets,
    wallet: &str,
    terms: &Terms,
    amount: u128,
    memo: &str,
) -> Result<Value, super::wallet::SignError> {
    use super::wallet::SignError;
    let owner = wallets
        .address(wallet)?
        .svm()
        .ok_or_else(|| SignError::Refused(format!("wallet {wallet} is not a Solana wallet")))?;
    let amount =
        u64::try_from(amount).map_err(|_| SignError::Refused("an amount past 64 bits".into()))?;
    let blockhash = match (terms.blockhash, &terms.rpc) {
        (Some(b), _) => b,
        (None, Some(url)) => latest_blockhash(url, RPC_TIMEOUT).map_err(SignError::Unavailable)?,
        (None, None) => return Err(SignError::Refused("no blockhash to build on".into())),
    };
    let message = terms
        .message(&owner, amount, memo.as_bytes(), blockhash)
        .map_err(SignError::Refused)?
        .serialize();
    let sig = wallets.sign_svm(wallet, &message)?;
    let tx = transaction(&[[0u8; 64], sig], &message);
    Ok(json!({"transaction": crate::b64::std_pad(&tx)}))
}

/// A recent blockhash from the RPC endpoint `url` (`{{secret:…}}`
/// references in it resolved now): `getLatestBlockhash` at `finalized`
/// commitment, within `timeout`. The URL is dialled by name, as an
/// endpoint the operator configured; an error names it without its query.
pub fn latest_blockhash(url: &str, timeout: Duration) -> Result<[u8; 32], String> {
    let shown = crate::config::settings::url_locator(url);
    let url = crate::sec::secret::resolve(url, &|k| std::env::var(k).ok())
        .map_err(|e| format!("security.payments.rpc {shown}: {e}"))?;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "getLatestBlockhash",
                      "params": [{"commitment": "finalized"}]})
    .to_string();
    let reply = crate::auth::http::send(
        "POST",
        &url,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
        timeout,
        crate::auth::http::Dial::Named,
    )
    .map_err(|e| format!("the RPC endpoint {shown}: {}", e.replace(&url, &shown)))?;
    if !reply.is_success() {
        return Err(format!(
            "the RPC endpoint {shown} answered {}",
            reply.status
        ));
    }
    if reply.body.len() > MAX_RPC_ANSWER {
        return Err(format!(
            "the RPC endpoint {shown} answered more than {MAX_RPC_ANSWER} bytes"
        ));
    }
    let v: Value = serde_json::from_slice(&reply.body)
        .map_err(|_| format!("the RPC endpoint {shown} answered no JSON"))?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let msg: String = e["message"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        return Err(format!(
            "the RPC endpoint {shown} refused getLatestBlockhash: {msg}"
        ));
    }
    v["result"]["value"]["blockhash"]
        .as_str()
        .and_then(|b| base58::decode32(b).ok())
        .ok_or_else(|| format!("the RPC endpoint {shown} answered no blockhash"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The x402 SVM specification's example entry: USDC on mainnet.
    pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    pub const PAY_TO: &str = "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4";
    pub const FEE_PAYER: &str = "EwWqGE4ZFKLofuestmU4LDdK7XM1N4ALgdZccwYugwGd";
    pub const BLOCKHASH: &str = "EZ3rST5dvHmbanh75jc4PuLfV96vp9fEYBVeNk4FfM1k";
    pub const MAINNET: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";

    fn pk(s: &str) -> Pubkey {
        Pubkey::parse(s).unwrap()
    }

    /// The terms of the specification's example, with its memo.
    pub fn terms() -> Terms {
        Terms {
            mint: pk(USDC),
            program: TokenProgram::Spl,
            decimals: 6,
            pay_to: pk(PAY_TO),
            fee_payer: pk(FEE_PAYER),
            memo: Some("pi_3abc123def456".into()),
            blockhash: Some(base58::decode32(BLOCKHASH).unwrap()),
            rpc: None,
        }
    }

    /// The vectors `@solana/web3.js` and `@solana/spl-token` give.
    const WEB3_MESSAGE: &str = "8002010408cf1f0667034d53d82dc7b650234c4542e13fcf2d800daee8e0c01b29877266daea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c67302e491894d7492ea6be4f914ea4f45fa142e645867c9164a276d5dd76f076258b940e3ee7b4667b15d05ad8aa3f006bf403891cd439586543b3c899f60f000306466fe5211732ffecadba72c39be7bc8ce5bbc5f7126b2c439b3a4000000006ddf6e1d765a193d9cbe146ceeb79ac1cb485ed5f5b37913a8cf5857eff00a9c6fa7af3bedbad3a3d65f36aabc97431b1bbe4c2d2f6e0e47ca60203452f5d61054a535a992921064d24e87160da387c7c35b5ddbc92bb81e41fa8404105448dc95dbdbb9b39b5bf72c576b12f12233c1fef72ff818488f73834d2f182d896eb0404000502204e00000400090301000000000000000504020603010a0ce8030000000000000607001070695f3361626331323364656634353600";
    const WEB3_TRANSACTION: &str = "AgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAV4XuAW9n+2FT9EfB9qLy9Z7mRExYUIh1OmwWZ9ftlIMdk/TPmhqIBmcm1VvfygEEmO5NsU+fJJibJEhdOa0ALgAIBBAjPHwZnA01T2C3HtlAjTEVC4T/PLYANrujgwBsph3Jm2upKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIsZzAuSRiU10kupr5PkU6k9F+hQuZFhnyRZKJ21d128HYli5QOPue0ZnsV0FrYqj8Aa/QDiRzUOVhlQ7PImfYPAAMGRm/lIRcy/+ytunLDm+e8jOW7xfcSayxDmzpAAAAABt324ddloZPZy+FGzut5rBy0he1fWzeROoz1hX7/AKnG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQVKU1qZKSEGTSTocWDaOHx8NbXdvJK7geQfqEBBBUSNyV29u5s5tb9yxXaxLxIjPB/vcv+BhIj3ODTS8YLYlusEBAAFAiBOAAAEAAkDAQAAAAAAAAAFBAIGAwEKDOgDAAAAAAAABgcAEHBpXzNhYmMxMjNkZWY0NTYA";

    fn unhex(h: &str) -> Vec<u8> {
        (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// Compact-u16 as Solana writes it, and only its shortest form read.
    #[test]
    fn compact_u16_is_solanas() {
        for (n, hex) in [
            (0u16, "00"),
            (0x7f, "7f"),
            (0x80, "8001"),
            (0x3fff, "ff7f"),
            (0x4000, "808001"),
            (0xffff, "ffff03"),
        ] {
            let mut out = Vec::new();
            compact_u16(n, &mut out);
            assert_eq!(crate::sha::to_hex(&out), hex, "{n}");
            let mut at = 0;
            assert_eq!(read_compact_u16(&out, &mut at), Ok(n));
            assert_eq!(at, out.len());
        }
        for bad in ["80", "8000", "ff8000", "ffff04", "808080"] {
            let mut at = 0;
            assert!(read_compact_u16(&unhex(bad), &mut at).is_err(), "{bad}");
        }
    }

    /// Associated token accounts as `@solana/spl-token` derives them, under
    /// both token programs — bumps of 255, 254 and 253 — and for owners
    /// whose first bumps all land on the curve (250, 249, 248), which only
    /// a right point check finds.
    #[test]
    fn associated_token_accounts_are_spl_tokens() {
        let usdc = pk(USDC);
        for (owner, program, ata, bump) in [
            (
                PAY_TO,
                TokenProgram::Spl,
                "3XZXfFJHF5ox3yPop16oqYfSWxLpkjsEuvTe2S67G2rj",
                254,
            ),
            (
                PAY_TO,
                TokenProgram::Token2022,
                "H7BrdQ48x8XkXh3UXs7pvPxxF8Z5Ej1WyBwU2ZryzTBL",
                255,
            ),
            (
                FEE_PAYER,
                TokenProgram::Spl,
                "2pgEYY7LqjJV3qyhsipqvYMWDVg9ro7uVrFMnACmc6Tn",
                254,
            ),
            (
                "11111111111111111111111111111111",
                TokenProgram::Spl,
                "HJt8Tjdsc9ms9i4WCZEzhzr4oyf3ANcdzXrNdLPFqm3M",
                255,
            ),
            (
                "11111111111111111111111111111111",
                TokenProgram::Token2022,
                "9m6XGSYPF8rpUNdRkA19vxfTa253RQyfmuBSGSArzCMP",
                254,
            ),
            (
                "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
                TokenProgram::Token2022,
                "5USYfwdBgN3CCyNUik1az46v13PXbVesaUpJmZpgDG3R",
                253,
            ),
            (
                "4zvwRjXUKGfvwnParsHAS3HuSVzV5cA4McphgmoCtajS",
                TokenProgram::Spl,
                "DJcjpsHnWXSucjUpourygEN3mkcQwSHG6d5b2AzLSfSn",
                250,
            ),
            (
                "HygQsxGeod8ifU5oABzvFxdu4YDDSkZKAYhGtKjg6CCh",
                TokenProgram::Spl,
                "2UtSmiU89qU7enEmFSUvJ3pyRvRXUWYGEcHWKmr9C5qG",
                249,
            ),
            (
                "BizYnoXeKxMmk5L2sMwo9tScHKvcvyJudND75WRbN5Un",
                TokenProgram::Spl,
                "5nVSc8ME5xJCNwtX4ovhJvZ3eXddgSMBmhdJSPL4vbY",
                248,
            ),
        ] {
            let o = pk(owner);
            assert_eq!(
                associated_token_account(&o, &usdc, program).to_string(),
                ata,
                "{owner}"
            );
            let seeds: [&[u8]; 3] = [&o.0, &program.id().0, &usdc.0];
            let (_, b) =
                find_program_address(&seeds, &Pubkey::known(ASSOCIATED_TOKEN_PROGRAM)).unwrap();
            assert_eq!(b, bump, "{owner}");
        }
        // More than 16 seeds, or a seed past 32 bytes, derives nothing.
        let long = [0u8; 33];
        assert!(create_program_address(&[&long], &usdc).is_none());
        let many: Vec<&[u8]> = vec![b"a"; 17];
        assert!(create_program_address(&many, &usdc).is_none());
    }

    /// The transaction x402's reference client builds, byte for byte as
    /// `@solana/web3.js` compiles, serialises and signs it: the
    /// specification's example entry, the wallet of the seed `07` × 32.
    /// Ed25519 is deterministic, so the signature is fixed bytes too, and
    /// verifies; the fee payer's is left zero.
    #[test]
    fn the_transaction_is_web3js_byte_for_byte() {
        let kp = key_pair(&[7u8; 32]).unwrap();
        let owner = pubkey_of(&kp);
        assert_eq!(
            owner.to_string(),
            "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB"
        );
        let t = terms();
        let m = t
            .message(&owner, 1000, b"pi_3abc123def456", t.blockhash.unwrap())
            .unwrap();
        let bytes = m.serialize();
        assert_eq!(crate::sha::to_hex(&bytes), WEB3_MESSAGE);
        assert_eq!(
            m.keys
                .iter()
                .map(Pubkey::to_string)
                .collect::<Vec<_>>()
                .join(" "),
            "EwWqGE4ZFKLofuestmU4LDdK7XM1N4ALgdZccwYugwGd GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB 7woc3ajaGMMXczFYjxon4aQoHH3j126fMUR9c58eHRsK 3XZXfFJHF5ox3yPop16oqYfSWxLpkjsEuvTe2S67G2rj ComputeBudget111111111111111111111111111111 TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
        );
        let sig = sign(&kp, &bytes);
        assert_eq!(
            base58::encode(&sig),
            "SNe5aH6rAgWvGyiEuKGFgcZVGgqCCci2tdoMDPJkj5ffPjyFttzQaX4G8BcT6dBZyLja22udYQKgHSr67FwxWYz"
        );
        assert!(verify(&owner, &bytes, &sig));
        assert!(!verify(&owner, &bytes[1..], &sig));
        let tx = transaction(&[[0u8; 64], sig], &bytes);
        assert_eq!(crate::b64::std_pad(&tx), WEB3_TRANSACTION);
        // And it reads back.
        assert_eq!(Message::parse(&bytes).unwrap(), m);
        let read = Transaction::read(&tx).unwrap();
        assert_eq!(read.signatures, vec![[0u8; 64], sig]);
        assert_eq!((&read.message, &read.signed), (&m, &bytes));
        let tr = Transfer::read(&read.message).unwrap();
        assert_eq!(
            (tr.amount, tr.decimals, tr.owner, tr.fee_payer),
            (1000, 6, owner, pk(FEE_PAYER))
        );
        assert_eq!(
            (tr.unit_limit, tr.unit_price),
            (COMPUTE_UNIT_LIMIT, COMPUTE_UNIT_PRICE)
        );
        assert_eq!(tr.memo, b"pi_3abc123def456");
        assert!(tr.pays(&pk(PAY_TO)));
        assert!(!tr.pays(&pk(FEE_PAYER)));
    }

    /// A message of any other shape is no transfer agentd signs, and says
    /// what differs; a message that does not parse is refused as bytes.
    #[test]
    fn only_the_exact_transfer_reads_as_one() {
        let kp = key_pair(&[7u8; 32]).unwrap();
        let owner = pubkey_of(&kp);
        let t = terms();
        let m = t.message(&owner, 1000, b"memo", [3u8; 32]).unwrap();
        assert!(Transfer::read(&m).is_ok());
        let with = |f: &dyn Fn(&mut Message)| {
            let mut x = m.clone();
            f(&mut x);
            Transfer::read(&x)
        };
        for (i, r) in [
            with(&|x| x.signers = 3),
            with(&|x| x.readonly_signed = 0),
            with(&|x| x.instructions.truncate(3)),
            with(&|x| {
                let extra = x.instructions[3].clone();
                x.instructions.push(extra);
            }),
            with(&|x| x.instructions.swap(0, 1)),
            with(&|x| x.instructions[2].accounts[3] = 0),
            with(&|x| x.instructions[2].accounts.push(0)),
            with(&|x| x.instructions[2].data[0] = 3),
            with(&|x| x.instructions[2].data.push(0)),
            with(&|x| x.instructions[0].data = vec![2, 0, 0]),
            with(&|x| x.instructions[1].data[0] = 4),
            with(&|x| x.instructions[3].accounts.push(2)),
            with(&|x| x.instructions[3].data = vec![0xff, 0xfe]),
            with(&|x| x.instructions[3].data = vec![b'a'; MAX_MEMO + 1]),
            with(&|x| x.instructions[2].accounts.swap(0, 2)),
            with(&|x| x.instructions[2].program = 4),
            with(&|x| x.keys.push(Pubkey([5; 32]))),
            // A ninth key no instruction names, read-only.
            with(&|x| {
                x.keys.push(Pubkey([5; 32]));
                x.readonly_unsigned += 1;
            }),
            with(&|x| x.keys[6] = x.keys[3]),
        ]
        .iter()
        .enumerate()
        {
            assert!(r.is_err(), "case {i} must not read as a transfer");
        }
        // A source that is not the wallet's own token account.
        let mut other = m.clone();
        other.keys[2] = Pubkey([9; 32]);
        assert!(
            Transfer::read(&other)
                .unwrap_err()
                .contains("associated token account")
        );
        // The fee payer is never the wallet.
        let mut own = t.clone();
        own.fee_payer = owner;
        assert!(
            own.message(&owner, 1, b"m", [3; 32])
                .unwrap_err()
                .contains("no network fee")
        );
        // Bytes: a legacy message, another version, a lookup table, a
        // trailing byte, an index past the keys.
        let bytes = m.serialize();
        let mut legacy = bytes[1..].to_vec();
        assert!(Message::parse(&legacy).unwrap_err().contains("legacy"));
        legacy.insert(0, 0x81);
        assert!(Message::parse(&legacy).unwrap_err().contains("version-1"));
        let mut lookups = bytes.clone();
        *lookups.last_mut().unwrap() = 1;
        assert!(Message::parse(&lookups).unwrap_err().contains("lookup"));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(Message::parse(&trailing).is_err());
        let mut past = m.clone();
        past.instructions[3].program = 8;
        assert!(Message::parse(&past.serialize()).is_err());
        assert!(Message::parse(&bytes[..bytes.len() - 2]).is_err());
        // A transaction whose signatures do not match its signers.
        let tx = transaction(&[[0u8; 64]], &bytes);
        assert!(Transaction::read(&tx).unwrap_err().contains("signers"));
    }

    /// An entry's Solana terms: the specification's example reads, with
    /// its memo and blockhash; with no blockhash it needs the network's RPC
    /// endpoint; no feePayer, or a feePayer or memo that does not read, is
    /// each a reason; a payee the caller could not read gives no terms.
    #[test]
    fn an_entrys_terms_read_as_the_specification_writes_them() {
        let entry = json!({"scheme": "exact", "network": MAINNET, "amount": "1000",
            "asset": USDC, "payTo": PAY_TO, "maxTimeoutSeconds": 60,
            "extra": {"feePayer": FEE_PAYER, "memo": "pi_3abc123def456",
                      "recentBlockhash": BLOCKHASH, "lastValidBlockHeight": "291470237"}});
        let read = |v: &Value, rpc: Option<&str>| {
            let mut why = Vec::new();
            let e = Entry::parse(v).unwrap();
            let pay_to = Pubkey::parse(&e.pay_to).ok();
            let t = Terms::read(&e, pk(USDC), TokenProgram::Spl, 6, pay_to, rpc, &mut why);
            (t, why.join("; "))
        };
        assert_eq!(read(&entry, None).0.unwrap(), terms());
        let with = |f: &dyn Fn(&mut Value)| {
            let mut e = entry.clone();
            f(&mut e);
            e
        };
        let no_hash = with(&|e| {
            e["extra"]
                .as_object_mut()
                .unwrap()
                .remove("recentBlockhash");
        });
        assert!(read(&no_hash, None).1.contains("no recentBlockhash"));
        let t = read(&no_hash, Some("https://rpc.example")).0.unwrap();
        assert_eq!(
            (t.blockhash, t.rpc.as_deref()),
            (None, Some("https://rpc.example"))
        );
        let bad_hash = with(&|e| e["extra"]["recentBlockhash"] = json!("0OIl"));
        assert!(
            read(&bad_hash, Some("https://rpc.example"))
                .0
                .unwrap()
                .blockhash
                .is_none()
        );
        for (e, want) in [
            (
                with(&|e| {
                    e["extra"].as_object_mut().unwrap().remove("feePayer");
                }),
                "no feePayer",
            ),
            (
                with(&|e| {
                    e["extra"]["feePayer"] = json!("0x209693Bc6afc0C5328bA36FaF03C514EF312287C")
                }),
                "extra.feePayer",
            ),
            (
                with(&|e| e["extra"]["memo"] = json!("x".repeat(MAX_MEMO + 1))),
                "extra.memo",
            ),
            (with(&|e| e["extra"]["memo"] = json!(7)), "extra.memo"),
        ] {
            let (t, why) = read(&e, None);
            assert!(t.is_none() && why.contains(want), "{want}: {why}");
        }
        let e = Entry::parse(&entry).unwrap();
        let mut why = Vec::new();
        assert!(Terms::read(&e, pk(USDC), TokenProgram::Spl, 6, None, None, &mut why).is_none());
        assert!(why.is_empty(), "the caller has said why");
        // No memo from the server: 16 random bytes in hex.
        let mut t = terms();
        t.memo = None;
        assert_eq!(t.memo_for(&[0xab; 32]), "ab".repeat(MEMO_NONCE));
    }

    /// A key's JWK names its address; a private member or another curve is
    /// refused.
    #[test]
    fn an_ed25519_jwk_names_its_address() {
        let owner = pubkey_of(&key_pair(&[7u8; 32]).unwrap());
        let jwk = jwk_of(&owner);
        assert_eq!(pubkey_of_jwk(&jwk).unwrap(), owner);
        let mut private = jwk.clone();
        private["d"] = json!("AA");
        assert!(pubkey_of_jwk(&private).is_err());
        let mut x25519 = jwk.clone();
        x25519["crv"] = json!("X25519");
        assert!(pubkey_of_jwk(&x25519).is_err());
        let mut short = jwk;
        short["x"] = json!("AAAA");
        assert!(pubkey_of_jwk(&short).is_err());
    }

    /// `getLatestBlockhash` against a one-shot local endpoint: the request
    /// is the JSON-RPC call, the answer's blockhash read; an error answer,
    /// a non-2xx and a blockhash that does not read are refusals.
    #[test]
    fn the_latest_blockhash_is_asked_of_the_rpc_endpoint() {
        use std::io::{Read, Write};
        let serve = |answer: String, status: u16| {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let h = std::thread::spawn(move || {
                let (mut s, _) = l.accept().unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !String::from_utf8_lossy(&req).contains("getLatestBlockhash") {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .unwrap();
                String::from_utf8_lossy(&req).to_string()
            });
            (format!("http://127.0.0.1:{port}/rpc"), h)
        };
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"context": {"slot": 1},
                        "value": {"blockhash": BLOCKHASH, "lastValidBlockHeight": 291470237}}});
        let (url, h) = serve(ok.to_string(), 200);
        let got = latest_blockhash(&url, Duration::from_secs(5)).unwrap();
        assert_eq!(base58::encode(&got), BLOCKHASH);
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /rpc "), "{req}");
        assert!(
            req.contains(r#""method":"getLatestBlockhash""#)
                && req.contains(r#""commitment":"finalized""#)
        );
        for (answer, status, want) in [
            (json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32005, "message": "node is behind"}}).to_string(), 200, "node is behind"),
            ("{}".to_string(), 503, "answered 503"),
            (json!({"result": {"value": {"blockhash": "0x00"}}}).to_string(), 200, "no blockhash"),
        ] {
            let (url, h) = serve(answer, status);
            let e = latest_blockhash(&url, Duration::from_secs(5)).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
            h.join().unwrap();
        }
    }
}
