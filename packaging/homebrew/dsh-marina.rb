# DSH Marina Homebrew Cask 模板。
#
# 这是放在独立 tap 仓库 QuaonY/homebrew-dsh-marina 里的 Cask 模板，
# 本仓库只保存模板，不自动推送到 tap。发布新版后：
#   1) 把 version 改成新版本号；
#   2) 用 xtask 产出的 dsh-marina-v<版本>-macos-aarch64.dmg.sha256 填入 sha256
#      （替换 :no_check，便于复现校验）；
#   3) 提交到 tap 仓库（由维护者手动 git 操作）。
#
# 用户安装：
#   brew tap QuaonY/dsh-marina
#   brew install --cask dsh-marina
#
# 免费 ad-hoc 签名（无 Apple Developer 账号）：Homebrew 4.7+ 不再支持
# --no-quarantine，首次打开仍需在「系统设置 → 隐私与安全性」放行（见 README）。
cask "dsh-marina" do
  version "0.1.3"
  sha256 :no_check

  url "https://github.com/QuaonY/dsh-marina/releases/download/v#{version}/dsh-marina-v#{version}-macos-aarch64.dmg"
  name "DSH Marina"
  desc "Multi-instance launcher for DeepSeek Harness"
  homepage "https://github.com/QuaonY/dsh-marina/"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :ventura"
  depends_on arch: :arm64

  app "DSH Marina.app"

  zap trash: "~/.dsh-marina"
end