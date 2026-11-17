# Native Rust Bridge

このリポジトリはRust版専用です。実行時にPython版を呼び出しません。元のPython版は[別リポジトリ](https://github.com/ryuya0124/xa401-mirakurun-bridge)に資料として保存し、稼働には使用しません。KonomiTV本体は独立したまま維持します。

Rust単独でSSDP、機器UDN・model照合、ContentDirectory、原画質resource選択、Full AKE、証明書・SRM検証、RTT応答、PCP復号、188/192バイトTS正規化、同局共有受信、SIT番組情報、PC録画・予約・管理画面、XA401録画の直接再生・時間シーク、KonomiTVの録画・予約API連携を実装しています。Moveや資格情報発行は実装していません。資格情報は明示した外部ファイルから読みます。

ビルド・CI・DockerはRust 1.100.0を使用します。ローカル環境は`rust-toolchain.toml`で同じバージョンを選択します。

OpenSSLは公式の4.1.0ソースをSHA-256照合してビルドし、静的にリンクします。Cargoの旧vendored版は使いません。Linux/macOSでは次の準備を行います。

```sh
sh scripts/build-openssl.sh
. ./openssl-build.conf
export OPENSSL_DIR="$PWD/target/openssl-$OPENSSL_VERSION/install"
export OPENSSL_STATIC=1
```

WindowsではMSVCとStrawberry Perlを導入後、PowerShellで`./scripts/build-openssl.ps1`を実行すると同じシェルのビルド環境が設定されます。ソースをUTF-8としてコンパイルし、日本語コードページによるC4819も防ぎます。OpenSSL 4.1の標準MSVC設定はデバッグ情報をオブジェクトへ含めるため、旧版の`ossl_static.pdb`欠落によるLNK4099を回避できます。警告抑制やOpenSSLソースの独自パッチは使いません。

```sh
cargo build --release --locked
target/release/xa401-bridge version
target/release/xa401-bridge discover --source 192.168.1.10
target/release/xa401-bridge info --config /private/bridge.json
target/release/xa401-bridge list --config /private/bridge.json --recorded
target/release/xa401-bridge capture --config /private/bridge.json --service SERVICE_ID --seconds 15 --output sample.ts
target/release/xa401-bridge serve --config /private/bridge.json
```

設定はPython版と共通のキー（location、udn、source、credentials、state_dir、folders、quality、listen、port、max_streams、recording、epg_collection）を使います。相対ファイルパスは設定ファイルのディレクトリを基準に解決します。画面のHTML・JS・WASMはコンパイル時に同梱します。

KonomiTV本体とそのAPI形式は維持しています。標準の新規EPG予約・キーワード予約やXA401本体の予約・削除操作は従来同様に未対応です。PC録画予約は既存の管理画面から作成します。番組表は受信したSITの現在・直近の情報で、8日分の将来EPGではありません。

## 接続と保存

Mirakurun互換APIはloopback限定です。TLSはアクセス元ACL・Host/Origin検査を設定した別のリバースプロキシで終端します。Python版の`lan_https`はRust版では使いません。既存のNginxを使う場合、バックエンドを同一ホストの`127.0.0.1:40772`にし、転送先Hostを`localhost:40772`へ設定してください。

KonomiTVのフロント全体も中継する場合だけ、信頼するプロキシから`X-XA401-Frontend: 1`と`X-Forwarded-Host`、`X-Forwarded-Proto`を付けます。元のOriginをプロキシで検査してから取り除いてください。この目印のないloopbackリクエストはMirakurun互換APIです。KonomiTVへの転送先は固定の`http://127.0.0.77:7010`です。

Windows / macOSは上記のOpenSSL準備後、各OS向けに`cargo build --release --locked`でビルドできます。WindowsではMSVCのC++ビルド環境とStrawberry Perl、macOSではXcode Command Line Toolsが必要です。OpenSSLはソースからビルドしてバイナリーへ組み込みます。HTML・JS・WASMも同梱するため、実行時のNode.jsも不要です。

LinuxのDocker構成は[deploy/compose.yml](deploy/compose.yml)です。PRIVATE_DIR・STATE_DIR・RECORDINGS_DIR・FFMPEG_DIRにホスト上の専用フォルダーを指定し、コンテナー内の設定はcredentials=`/private/credentials.env`、state_dir=`/data/state`、recording.directory=`/data/recordings`、ffmpeg/ffprobe=`/media-tools/ffmpeg.elf` / `/media-tools/ffprobe.elf`へ合わせます。録画先は容量の大きい既存パーティションにしてください。

```sh
docker compose -f deploy/compose.yml up -d --build
```

配布イメージにはRust実行ファイルとDebianの実行環境だけを含め、コンパイラー・ビルドキャッシュ・Python Bridgeは含めません。Dockerビルド時にRustの検査を実行します。188バイトTSの正規化はバッファーを共有し、同梱Webファイルの配信も静的データを共有します。文字列変換の一文字ごとの割当を減らし、固定KonomiTV転送先のHTTP接続プールは上限付きで再利用します。認証・品質設定・受信枠数は維持しています。これらの変更によるCPU・メモリーの改善率は未測定です。

既存KonomiTVの録画UI拡張は[互換パッチ](deploy/konomitv-external-recordings.patch)で再現できます。KonomiTVは独自の配布・管理手順に従ってください。

録画保存先は`recording.directory`、空き容量の下限は`recording.min_free_bytes`、1ファイルの上限は`recording.max_file_bytes`です。ffprobe/ffmpegへの明示したパスが必要です。録画は再圧縮せず、末尾の不完全PESを取り除いた後、映像・音声の実デコードに通ったものだけを`.ts`として公開します。失敗したデータは`.part`等で保持し、完成扱いにしません。

同じXA401に対して本番版と別の検証受信を同時稼働しないでください。受信枠が競合して通常視聴が拒否された実例があります。稼働中の1つのBridge APIに集約すれば、同局は共有し、2つの異なるチャンネルまでを管理し、番組情報の収集は視聴に受信枠を譲ります。

暗号・AKE・SRMの処理と公開定数はMITの[pydtcpip](https://github.com/tsukumijima/pydtcpip/tree/0114ed187d86902a879bcd775358cbb0ab917ba7)を移植しています。権利表示は[licenses/pydtcpip-MIT.txt](licenses/pydtcpip-MIT.txt)。暗号演算はOpenSSL、番組文字列はMITのarib crate。実資格情報をソースへ埋め込まないでください。

## 検証（2026-11-16）

- Intel N100 DebianでRust単独Full AKE・放送TS取得。12秒24,636,648バイト、188TS、MPEG-2 1920×1080/AAC、FFmpeg 3秒デコードexit 0。開始途中のGOP警告あり。
- 同じTSのSITをPython版と比較し、番組ID・時刻・長さ・タイトル・説明・拡張説明・ジャンル・放送CA区分が一致。
- PC予約の作成・一覧・取消、15秒録画約31.8MBの完了、5秒の映像・音声デコードとRange読み出しが成功。別の開始位置ではAACデコードに失敗し、未完成データとして保持した。全番組・全開始位置の成功は保証しない。
- 2局・3クライアントで同局の受信共有と3局目の503を確認。12.0秒の測定でRust CPUは約0.018コア相当、RSS約35.0MB。ホスト全体CPUは約17.8%（他サービス含む）。Original受信の短時間測定で、GPU再圧縮や長時間安定性の測定ではない。
- 通常のKonomiTV録画は元のschemaで検証し、外部録画の拡張データはPython版と全項目一致。未知のcodec/CA情報を埋めて純正schemaに見せかけない。

軽量化変更後に11個のRust検査と7個のブラウザー再生制御のNode検査を実行。N100 Dockerへの配備後も実放送8,460,000バイトの188バイト同期・scramble bits 0・MPEG2/AACの映像音声デコードexit 0を確認しました。移植前のPython版65検査も当時成功していますが、このリポジトリではPython検査やPython受信を実行しません。依存関係はCargo.lockで固定しています。OSVには公開crate名・バージョンだけを送って確認します。検出0は既知アドバイザリーの確認結果で、完全な安全性の保証ではありません。
