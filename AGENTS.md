# Agent Entry Point (RemoteAudio)

This repository's operational guidance is maintained in `CLAUDE.md`.

- プロジェクト概要・ルール: `./CLAUDE.md`
- ユーザー向けドキュメント: `./README.md`
- ローカル/プライベート追記（存在する場合・コミットしない）: `./CLAUDE.local.md` / `./AGENTS.local.md` / `./docs/local/`

個人/グローバル AI ルールは意図的にこのリポジトリの外に置く。各 AI ツールの
グローバル設定を使うこと。本ファイルは fresh public clone でも有効に保つ。

## Non-negotiables (full detail in CLAUDE.md)

<!-- TODO: プロジェクト固有の絶対ルールを 2〜4 個。例:
- 実データ（PII・本番 ID・トークン）は絶対にコミットしない
- 既定は dry-run。実操作は明示フラグ必須
- 公開 fixture はダミーのみ
-->

- ビルド・コミット禁止、secrets-scan 責務、plan/bugfix/pending md の作成ルール等の AI 作業共通ルールは、各利用者のグローバル AI 設定に従う（作者環境の例: `~/.claude/CLAUDE.md` および `~/.claude/guides/`）
- secrets-scan のこのリポジトリの配線（scanner パス・手動実行コマンド等）は `CLAUDE.md` の「secrets-scan（このリポジトリの配線）」節を参照

ガイダンス間で矛盾が出たら `CLAUDE.md` を優先する。

<!-- many-ai-cli の承認マーカーブロックはここに自動注入される。本ファイルでは持たない。 -->

## ファイル索引・テーブル逆引き（探す前に読む）

**どのファイルが何をして、どのテーブルを読み書きするかを聞かれたら、grep で探し回る前に
`.omitnix/index.json` を読む。** 全ファイルの索引とテーブルからの逆引きが入っている。

- **解析できなかったファイルも名前と理由付きで載っている。** 「索引に無い」と「読めなかった」を
  取り違えない。参照 0 件は「未使用」ではない
- `generated.commit` が現在の HEAD と違えば、索引はその commit 時点のもの。
  **古いまま断定せず、古いことを添えて答えるか `omitnix` で作り直す**
