// Point a registered protocol's alerts somewhere else.
//   ANCHOR_PROVIDER_URL=… ANCHOR_WALLET=… yarn ts-node scripts/set-alert-url.ts <program_id> <alert_url>
import * as anchor from "@anchor-lang/core";
import { Program } from "@anchor-lang/core";
import { PublicKey } from "@solana/web3.js";
import { DependencyRegistry } from "../target/types/dependency_registry";
import idl from "../target/idl/dependency_registry.json";

async function main() {
  const [programIdArg, alertUrl] = process.argv.slice(2);
  if (!programIdArg || !alertUrl) throw new Error("usage: <program_id> <alert_url>");
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = new Program<DependencyRegistry>(idl as DependencyRegistry, provider);
  const [protocol] = PublicKey.findProgramAddressSync(
    [Buffer.from("protocol"), new PublicKey(programIdArg).toBuffer()],
    program.programId
  );
  const sig = await program.methods
    .updateProtocol({ alertUrl, corpusSize: null })
    .accountsPartial({ authority: provider.wallet.publicKey, protocol })
    .rpc();
  const p = await program.account.protocol.fetch(protocol);
  console.log(`alert_url of ${protocol.toBase58()} is now ${p.alertUrl} (${sig})`);
}
main().catch((e) => { console.error(e); process.exit(1); });
