import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { assert } from "chai";
import { SvmscopeCrate } from "../target/types/svmscope_crate";

describe("svmscope reference program", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.svmscopeCrate as Program<SvmscopeCrate>;

  const counterPda = anchor.web3.PublicKey.findProgramAddressSync(
    [Buffer.from("counter")],
    program.programId
  )[0];

  function schedulePda(
    beneficiary: anchor.web3.PublicKey,
    scheduleId: anchor.BN
  ): anchor.web3.PublicKey {
    return anchor.web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("vesting"),
        beneficiary.toBuffer(),
        scheduleId.toArrayLike(Buffer, "le", 8),
      ],
      program.programId
    )[0];
  }

  async function fund(address: anchor.web3.PublicKey, lamports: number) {
    const signature = await provider.connection.requestAirdrop(
      address,
      lamports
    );
    await provider.connection.confirmTransaction(signature, "confirmed");
  }

  it("initializes and increments the counter", async () => {
    await program.methods
      .initializeCounter()
      .accountsPartial({
        signer: provider.wallet.publicKey,
        counter: counterPda,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .incrementCounter()
      .accountsPartial({
        signer: provider.wallet.publicKey,
        counter: counterPda,
      })
      .rpc();

    const counter = await program.account.counter.fetch(counterPda);
    assert.equal(counter.count.toString(), "1");
  });

  it("rejects a claim before the cliff", async () => {
    const beneficiary = anchor.web3.Keypair.generate();
    await fund(beneficiary.publicKey, 100_000_000);
    const scheduleId = new anchor.BN(Date.now());
    const schedule = schedulePda(beneficiary.publicKey, scheduleId);
    const now = Math.floor(Date.now() / 1000);

    await program.methods
      .createVesting(
        scheduleId,
        new anchor.BN(10_000_000),
        new anchor.BN(now),
        new anchor.BN(now + 86_400),
        new anchor.BN(now + 30 * 86_400)
      )
      .accountsPartial({
        creator: provider.wallet.publicKey,
        beneficiary: beneficiary.publicKey,
        schedule,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();

    try {
      await program.methods
        .claimVested(scheduleId)
        .accountsPartial({ beneficiary: beneficiary.publicKey, schedule })
        .signers([beneficiary])
        .rpc();
      assert.fail("pre-cliff claim unexpectedly succeeded");
    } catch (error) {
      assert.include(String(error), "CliffNotReached");
    }
  });

  it("claims a fully matured schedule and closes it", async () => {
    const beneficiary = anchor.web3.Keypair.generate();
    await fund(beneficiary.publicKey, 100_000_000);
    const scheduleId = new anchor.BN(Date.now() + 1);
    const schedule = schedulePda(beneficiary.publicKey, scheduleId);
    const amount = new anchor.BN(10_000_000);
    const now = Math.floor(Date.now() / 1000);

    await program.methods
      .createVesting(
        scheduleId,
        amount,
        new anchor.BN(now - 30),
        new anchor.BN(now - 20),
        new anchor.BN(now - 10)
      )
      .accountsPartial({
        creator: provider.wallet.publicKey,
        beneficiary: beneficiary.publicKey,
        schedule,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();

    let state = await program.account.vestingSchedule.fetch(schedule);
    assert.equal(state.totalAmount.toString(), amount.toString());
    assert.equal(state.claimedAmount.toString(), "0");

    await program.methods
      .claimVested(scheduleId)
      .accountsPartial({ beneficiary: beneficiary.publicKey, schedule })
      .signers([beneficiary])
      .rpc();

    state = await program.account.vestingSchedule.fetch(schedule);
    assert.equal(state.claimedAmount.toString(), amount.toString());

    await program.methods
      .closeVesting(scheduleId)
      .accountsPartial({ creator: provider.wallet.publicKey, schedule })
      .rpc();
    assert.isNull(await provider.connection.getAccountInfo(schedule));
  });
});
