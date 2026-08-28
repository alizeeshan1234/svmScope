import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { SvmscopeCrate } from "./target/types/svmscope_crate";

async function main() {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.svmscopeCrate as Program<SvmscopeCrate>;
  const beneficiary = anchor.web3.Keypair.generate();
  const airdrop = await provider.connection.requestAirdrop(
    beneficiary.publicKey,
    100_000_000
  );
  await provider.connection.confirmTransaction(airdrop, "confirmed");

  const scheduleId = new anchor.BN(Date.now());
  const schedule = anchor.web3.PublicKey.findProgramAddressSync(
    [
      Buffer.from("vesting"),
      beneficiary.publicKey.toBuffer(),
      scheduleId.toArrayLike(Buffer, "le", 8),
    ],
    program.programId
  )[0];
  const now = Math.floor(Date.now() / 1000);
  const signature = await program.methods
    .createVesting(
      scheduleId,
      new anchor.BN(50_000_000),
      new anchor.BN(now),
      new anchor.BN(now + 7 * 86_400),
      new anchor.BN(now + 30 * 86_400)
    )
    .accountsPartial({
      creator: provider.wallet.publicKey,
      beneficiary: beneficiary.publicKey,
      schedule,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();

  console.log("create_vesting:", signature);
  console.log("schedule:", schedule.toBase58());
  console.log(
    "Run `cd scope-test && cargo run` for the full svmscope workflow."
  );
}

main()
  .then(() => process.exit(0))
  .catch((error) => {
    console.error(error);
    process.exit(1);
  });
