# 找一个能用的 JAVA_HOME。
#
# 为什么需要：环境里常有个坏掉的 JAVA_HOME（比如 /opt/android-studio/jbr
# 指向没装的 Android Studio），Gradle 会直接报
# "JAVA_HOME is set to an invalid directory"。
#
# 用法：在 recipe 里 `. scripts/jdk.sh`（要 bash，不要 sh）
if [ ! -x "${JAVA_HOME:-}/bin/java" ]; then
  _java_bin="$(command -v java || true)"
  if [ -n "$_java_bin" ]; then
    export JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$_java_bin")")")"
    echo "JAVA_HOME 无效，改用 $JAVA_HOME"
  else
    echo "找不到 java，请装 JDK 17+ 或设 AGENT_JAVA_HOME" >&2
    exit 1
  fi
fi
