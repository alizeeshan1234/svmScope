import * as anchor from "@anchor-lang/core";
import { Program } from "@anchor-lang/core";
import { Keypair, PublicKey } from "@solana/web3.js";
import { assert } from "chai";
import { execSync } from "child_process";
import { mkdtempSync, writeFileSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { DependencyRegistry } from "../target/types/dependency_registry";

const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

describe("dependency_registry", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace
    .dependencyRegistry as Program<DependencyRegistry>;
  const authority = provider.wallet;

  // The protocol under test is a second copy of this program, deployed in
  // `before` with the wallet as upgrade authority. (The workspace program is
  // loaded at genesis with no upgrade authority, so it cannot be registered.)
  const watchedKeypair = Keypair.generate();
  const watchedProgram = watchedKeypair.publicKey;
  const [programData] = PublicKey.findProgramAddressSync(
    [watchedProgram.toBuffer()],
    BPF_LOADER_UPGRADEABLE
  );
  const [protocol] = PublicKey.findProgramAddressSync(
    [Buffer.from("protocol"), watchedProgram.toBuffer()],
    program.programId
  );
  const depA = Keypair.generate().publicKey;
  const depB = Keypair.generate().publicKey;
  const dependencyPda = (dep: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("dependency"), protocol.toBuffer(), dep.toBuffer()],
      program.programId
    )[0];

  const stranger = Keypair.generate();

  async function expectError(promise: Promise<unknown>, code: string) {
    try {
      await promise;
    } catch (err) {
      const anchorErr = err as anchor.AnchorError;
      assert.equal(anchorErr.error?.errorCode?.code, code, String(err));
      return;
    }
    assert.fail(`expected error ${code}`);
  }

  before(async () => {
    const sig = await provider.connection.requestAirdrop(
      stranger.publicKey,
      2_000_000_000
    );
    await provider.connection.confirmTransaction(sig, "confirmed");

    // Deploy the watched program with the wallet as its upgrade authority.
    const dir = mkdtempSync(join(tmpdir(), "dependency-registry-"));
    const keypairPath = join(dir, "watched.json");
    writeFileSync(keypairPath, JSON.stringify(Array.from(watchedKeypair.secretKey)));
    execSync(
      `solana program deploy target/deploy/dependency_registry.so ` +
        `--program-id ${keypairPath} -u ${provider.connection.rpcEndpoint} ` +
        `--commitment confirmed`,
      { stdio: "pipe" }
    );
  });

  it("refuses registration by someone who is not the upgrade authority", async () => {
    await expectError(
      program.methods
        .register(watchedProgram, "https://example.com/alert", 200)
        .accounts({
          payer: stranger.publicKey,
          authority: stranger.publicKey,
          programData,
        })
        .signers([stranger])
        .rpc(),
      "NotUpgradeAuthority"
    );
  });

  it("refuses a program data account that belongs to another program", async () => {
    // Real program data (the watched program's), but registering a different id.
    await expectError(
      program.methods
        .register(Keypair.generate().publicKey, "https://example.com/alert", 200)
        .accounts({
          payer: authority.publicKey,
          authority: authority.publicKey,
          programData,
        })
        .rpc(),
      "ProgramDataMismatch"
    );
  });

  it("registers a protocol", async () => {
    await program.methods
      .register(watchedProgram, "https://example.com/alert", 200)
      .accounts({
        payer: authority.publicKey,
        authority: authority.publicKey,
        programData,
      })
      .rpc();

    const p = await program.account.protocol.fetch(protocol);
    assert.ok(p.authority.equals(authority.publicKey));
    assert.ok(p.programId.equals(watchedProgram));
    assert.equal(p.alertUrl, "https://example.com/alert");
    assert.equal(p.corpusSize, 200);
    assert.equal(p.dependencyCount, 0);
  });

  it("rejects an alert url over 128 bytes", async () => {
    await expectError(
      program.methods
        .updateProtocol({ alertUrl: "x".repeat(129), corpusSize: null })
        .accounts({ authority: authority.publicKey, protocol })
        .rpc(),
      "AlertUrlTooLong"
    );
  });

  it("adds two dependencies", async () => {
    for (const dep of [depA, depB]) {
      await program.methods
        .addDependency(dep)
        .accounts({
          payer: authority.publicKey,
          authority: authority.publicKey,
          protocol,
        })
        .rpc();
    }
    const p = await program.account.protocol.fetch(protocol);
    assert.equal(p.dependencyCount, 2);

    const d = await program.account.dependency.fetch(dependencyPda(depA));
    assert.ok(d.protocol.equals(protocol));
    assert.ok(d.programId.equals(depA));
    assert.equal(d.alertsEnabled, true);
    assert.equal(d.lastCheckedSlot.toNumber(), 0);
  });

  it("lists a protocol's dependencies by filter, the way the engine will", async () => {
    const all = await program.account.dependency.all([
      { memcmp: { offset: 8, bytes: protocol.toBase58() } },
    ]);
    const ids = all.map((a) => a.account.programId.toBase58()).sort();
    assert.deepEqual(ids, [depA.toBase58(), depB.toBase58()].sort());
  });

  it("refuses add_dependency from a non-authority signer", async () => {
    await expectError(
      program.methods
        .addDependency(Keypair.generate().publicKey)
        .accounts({
          payer: stranger.publicKey,
          authority: stranger.publicKey,
          protocol,
        })
        .signers([stranger])
        .rpc(),
      "Unauthorized"
    );
  });

  it("turns alerts off for one dependency", async () => {
    await program.methods
      .setAlerts(false)
      .accounts({
        authority: authority.publicKey,
        protocol,
        dependency: dependencyPda(depB),
      })
      .rpc();
    const d = await program.account.dependency.fetch(dependencyPda(depB));
    assert.equal(d.alertsEnabled, false);
  });

  it("updates alert url and corpus size", async () => {
    await program.methods
      .updateProtocol({ alertUrl: "https://example.com/v2", corpusSize: 500 })
      .accounts({ authority: authority.publicKey, protocol })
      .rpc();
    const p = await program.account.protocol.fetch(protocol);
    assert.equal(p.alertUrl, "https://example.com/v2");
    assert.equal(p.corpusSize, 500);
  });

  it("removes a dependency and refunds rent to the authority", async () => {
    const before = await provider.connection.getBalance(authority.publicKey);
    await program.methods
      .removeDependency()
      .accounts({
        authority: authority.publicKey,
        protocol,
        dependency: dependencyPda(depA),
      })
      .rpc();
    const after = await provider.connection.getBalance(authority.publicKey);
    assert.isAbove(after, before);

    const p = await program.account.protocol.fetch(protocol);
    assert.equal(p.dependencyCount, 1);
    assert.isNull(
      await provider.connection.getAccountInfo(dependencyPda(depA))
    );
  });

  it("transfers authority, after which the old authority is refused", async () => {
    await program.methods
      .transferAuthority()
      .accounts({
        authority: authority.publicKey,
        newAuthority: stranger.publicKey,
        protocol,
      })
      .rpc();
    const p = await program.account.protocol.fetch(protocol);
    assert.ok(p.authority.equals(stranger.publicKey));

    await expectError(
      program.methods
        .setAlerts(true)
        .accounts({
          authority: authority.publicKey,
          protocol,
          dependency: dependencyPda(depB),
        })
        .rpc(),
      "Unauthorized"
    );

    // and the new authority works
    await program.methods
      .setAlerts(true)
      .accounts({
        authority: stranger.publicKey,
        protocol,
        dependency: dependencyPda(depB),
      })
      .signers([stranger])
      .rpc();
  });
});
