# Homebrew cask for DBiji.
# Publish the notarized DMG produced by scripts/bundle-macos.sh to the release page,
# then update `version` and `sha256` (printed by the script) and submit to a tap:
#   brew tap dbiji/tap && brew install --cask dbiji
cask "dbiji" do
  version "0.3.9"
  sha256 "9aa65090f1b18407fd9eec64ec9b7f7055084a6aadf6c356b841d29152b2a954"

  url "https://github.com/ilhamnadhif/dbiji/releases/download/v#{version}/DBiji-#{version}.dmg"
  name "DBiji"
  desc "Lightweight SQL database client written in Rust"
  homepage "https://github.com/ilhamnadhif/dbiji"

  depends_on macos: ">= :monterey"

  app "DBiji.app"
  binary "#{appdir}/DBiji.app/Contents/MacOS/dbiji"

  zap trash: [
    "~/Library/Application Support/dev.dbiji.DBiji",
    "~/Library/Caches/dev.dbiji.DBiji",
    "~/Library/Preferences/dev.dbiji.DBiji.plist",
  ]
end
