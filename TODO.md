# TODO — design review findings

設計レビュー（2026-06）で挙がった改善点。重要度順。`file:line` は指摘時点の目安。

## 🔴 Critical — 主張と実装の乖離 / 安全性

- [x] **C1. 「git = memory」が実装に存在しない** — README/manual が "git = the loop's
      memory" と謳うが `git init/add/commit` はコードに皆無。
      → *対応済み: 主張を「memory is the files in the data dir」に撤回（#feat/remove-looop-run）。*
      将来 git 履歴（各tick=1コミット、ロールバック、`looop diff`）を入れるなら別タスクで再検討。
- [x] **C3. single-writer 不変条件を `looop run` が破る** — pulse 稼働中でも拒否せず
      並走し journal/goals/PLAYBOOK をレース。
      → *対応済み: `looop run <goal>` を全廃（#feat/remove-looop-run）。*
- [~] **C2. RULE 1「1 tick = 1 move」の強制** — typed-action 化で解決。decider は
      `.decision.json` に1アクションを emit し、**looop が唯一の executor**。モデルが
      暴走しても tick あたり1 move にコードで限定される（#feat/typed-action-executor）。
      残タスク（完全サンドボックス化）：
      - [ ] `run_shell` をコードでゲート（deny-list：rm -rf / git push / gh pr merge / kubectl delete 等）
      - [ ] decider のツール権限を落とす（stage B：調査は sensor、出力は .decision.json のみ）
      - [ ] `--dangerously-skip-permissions` を外す（上記が済めば不要に）

## 🟠 High — コスト暴走 / 堅牢性

- [ ] **H1. 失敗 tick にバックオフがない** — `.last-tick-hash` は成功時のみ書込のため、
      毎回失敗する tick が cadence ごとに無限リトライ＆無限課金。`src/tick.rs`
      - [ ] 連続失敗カウンタ + 指数バックオフ、または失敗時もhash書込 + 限定リトライ
- [ ] **H2. 予算上限 / サーキットブレーカがない** — `looop cost` で可視化のみ。
      config に `max_daily_usd` を追加し超過で tick skip + flag。`src/cost.rs`, `src/run.rs`
- [ ] **H3. claude ランナーの tick コストが計上されない** — pi は `| looop _fmt` を通すが
      claude tick は通さないため常にゼロ行。runner 間で会計挙動が非対称。`src/config.rs`
      - [ ] claude 側にも会計シームを足す、または挙動差をドキュメント明記
- [ ] **H4. `looop tick` が snapshots を共有してレースする** — `cmd_run_goal` は private temp を
      使っていたが `tick()`/`cmd_tick` は共有 `snapshots/` を wipe→再生成。二重起動 / cron 重複で
      互いの snapshots を消し合う。ロックも未取得。`src/tick.rs`
      - [ ] private snapshot dir 方式に統一、または `looop tick` でもロック取得

## 🟡 Medium — 整合性 / 設計

- [ ] **M1. 環境変数の名前空間汚染** — `export_env` が裸の `CONFIG` / `CLAIMS_DIR` /
      `REPORTS_DIR` / `COST_LEDGER` を全子プロセスに export。特に `CONFIG` は衝突の温床。
      `LOOOP_` プレフィックスへ統一（参照側の sensor/CONTRACT も移行）。`src/main.rs`
- [ ] **M2. `setup` ブートストラップが死んだ corpse で詰まる** — starter PLAYBOOK の
      "if a session setup already exists, do nothing" が Exited/Killed の corpse も
      「exists」と解釈し最大 retention(3d) 停止しうる。seed を "if a **live** setup session"
      に厳密化。`src/seed/PLAYBOOK.md`
- [ ] **M3. PLAYBOOK 編集は wake するが sensor 編集は wake しない（非対称）** —
      `world_hash` は sensors/*.sh をハッシュしない。意図的だが挙動差をドキュメント明記。
      `src/gate.rs`
- [ ] **M4. 判断は弱モデル・実行は強モデル** — tick=sonnet/low、worker=opus/medium。
      全体を左右する「どの一手か」を一番弱いモデルが下す配分の再考。`src/config.rs`
- [x] **M5. config がプロファイル非スコープ** — `LOOOP_DATA_DIR` で分離してもconfigは共有。
      → *対応済み: config を `<data_dir>/looop.json` に移動。プロファイルを分ければ
      config も自動で分かれる。明示の `$LOOOP_CONFIG` は引き続き最優先(配線共有用)。`src/paths.rs`*

## 🟢 Feature / UX

- [ ] **F1. `looop journal [--tail N]`** — 意思決定ログを一級コマンドで読む（今は cat か
      `log pulse`=イベント列のみ）。
- [ ] **F2. `looop tick --dry-run`** — プロンプトを組み立てて表示するだけで LLM を呼ばない
      （課金なしでプロンプト調整）。
- [ ] **F3. `max_daily_usd` サーキットブレーカ** — H2 と統合。
- [ ] **F4. `looop doctor`** — runner が PATH にあるか / config が parse できるか /
      データディレクトリ書込可か を診断。
- [ ] **F5. オンボーディング — runner 自動検出** — default が `pi` 固定 + モデル名ハードコード。
      claude しか無いユーザは config 編集必須。初回に検出。
