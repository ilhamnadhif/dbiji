# Homebrew cask for DBiji.
# Publish the notarized DMG produced by scripts/bundle-macos.sh to the release page,
# then update `version` and `sha256` (printed by the script) and submit to a tap:
#   brew tap dbiji/tap && brew install --cask dbiji
cask "dbiji" do
  version "0.3.27"
  sha256 "9788ee87fcb4795cce96021768ba7ddbed2d49e8bfc2200544feaf70081e8afe"

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
