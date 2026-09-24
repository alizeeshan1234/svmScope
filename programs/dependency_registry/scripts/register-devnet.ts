// Register a protocol on devnet and add dependencies.
//
//   ANCHOR_PROVIDER_URL=https://api.devnet.solana.com \
//   ANCHOR_WALLET=~/.config/solana/id.json \
//   yarn ts-node scripts/register-devnet.ts <program_id> <alert_url> <dep_program_id>...
//
// The wallet must be the upgrade authority of <program_id>.
import * as anchor from "@anchor-lang/core";
import { Program } from "@anchor-lang/core";
import { PublicKey } from "@solana/web3.js";
import { DependencyRegistry } from "../target/types/dependency_registry";
import idl from "../target/idl/dependency_registry.json";

const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

async function main() {
  const [programIdArg, alertUrl, ...deps] = process.argv.slice(2);
  if (!programIdArg || !alertUrl) {
    throw new Error("usage: <program_id> <alert_url> [dep_program_id...]");
  }
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = new Program<DependencyRegistry>(idl as DependencyRegistry, provider);

  const watched = new PublicKey(programIdArg);
  const [programData] = PublicKey.findProgramAddressSync(
    [watched.toBuffer()],
    BPF_LOADER_UPGRADEABLE
  );
  const [protocol] = PublicKey.findProgramAddressSync(
    [Buffer.from("protocol"), watched.toBuffer()],
    program.programId
  );

  const existing = await provider.connection.getAccountInfo(protocol);
  if (existing) {
    console.log(`protocol already registered at ${protocol.toBase58()}`);
  } else {
    const sig = await program.methods
      .register(watched, alertUrl, 200)
      .accountsPartial({
        payer: provider.wallet.publicKey,
        authority: provider.wallet.publicKey,
        programData,
      })
      .rpc();
    console.log(`registered ${watched.toBase58()} as ${protocol.toBase58()} in ${sig}`);
  }

  for (const dep of deps) {
    const depKey = new PublicKey(dep);
    const [dependency] = PublicKey.findProgramAddressSync(
      [Buffer.from("dependency"), protocol.toBuffer(), depKey.toBuffer()],
      program.programId
    );
    if (await provider.connection.getAccountInfo(dependency)) {
      console.log(`dependency ${dep} already present`);
      continue;
    }
    const sig = await program.methods
      .addDependency(depKey)
      .accountsPartial({
        payer: provider.wallet.publicKey,
        authority: provider.wallet.publicKey,
        protocol,
      })
      .rpc();
    console.log(`added dependency ${dep} as ${dependency.toBase58()} in ${sig}`);
  }

  const all = await program.account.dependency.all([
    { memcmp: { offset: 8, bytes: protocol.toBase58() } },
  ]);
  console.log(`protocol ${protocol.toBase58()} now has ${all.length} dependencies:`);
  for (const d of all) console.log(`  ${d.account.programId.toBase58()} alerts=${d.account.alertsEnabled}`);
}

main().catch((e) => { console.error(e); process.exit(1); });
