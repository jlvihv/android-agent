# 找一个能用的 JAVA_HOME。
#
# 为什么需要：环境里常有个坏掉的 JAVA_HOME（比如 /opt/android-studio/jbr
# 指向没装的 Android Studio），Gradle 会直接报
# "JAVA_HOME is set to an invalid directory"。
#
# 顺序：AGENT_JAVA_HOME → 现有 JAVA_HOME → macOS 的 java_home → Homebrew 的
# openjdk@17（keg-only，不在 PATH 上）→ PATH 里的 java。
# macOS 的 /usr/bin/java 是个没装 JDK 时只会报错的桩，不能信 `command -v java`。
#
# 用法：在 recipe 里 `. scripts/jdk.sh`（要 bash，不要 sh）
_ok() { [ -n "${1:-}" ] && "$1/bin/java" -version >/dev/null 2>&1; }

if _ok "${AGENT_JAVA_HOME:-}"; then
  export JAVA_HOME="$AGENT_JAVA_HOME"
elif ! _ok "${JAVA_HOME:-}"; then
  _cand=""
  if [ -x /usr/libexec/java_home ]; then
    _cand="$(/usr/libexec/java_home -v 17+ 2>/dev/null || true)"
  fi
  if ! _ok "$_cand" && command -v brew >/dev/null; then
    _cand="$(brew --prefix openjdk@17 2>/dev/null || true)/libexec/openjdk.jdk/Contents/Home"
  fi
  if ! _ok "$_cand"; then
    _java_bin="$(command -v java || true)"
    [ -n "$_java_bin" ] && _cand="$(dirname "$(dirname "$(readlink -f "$_java_bin")")")"
  fi
  if _ok "$_cand"; then
    export JAVA_HOME="$_cand"
    echo "JAVA_HOME 无效，改用 $JAVA_HOME"
  else
    echo "找不到 java，请装 JDK 17+ 或设 AGENT_JAVA_HOME" >&2
    exit 1
  fi
fi
