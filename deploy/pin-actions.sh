#!/bin/sh
# 把 .github/workflows 里引用的第三方组件（uses: 作者/仓库@版本）钉到具体的提交号。
#
# 为什么：@v4 这种版本标签是可以被挪动的，作者的账号一旦被盗，对方把标签指向一份带后门的代码，
# 下一次构建就会跑它，而构建是能推镜像的。钉成 40 位提交号以后，用的永远是这一份，谁也改不了。
#
# 用法：在仓库根目录执行 sh deploy/pin-actions.sh。需要 git 和能访问 GitHub 的网络。
# 每行改成「作者/仓库@提交号 # 版本」，后面的注释记着原来的版本。以后想升级，再跑一次：
# 按注释里的版本重新查一遍，标签挪到了新提交就换成新的。
set -eu
cd "$(dirname "$0")/.."

FILES=$(ls .github/workflows/*.yml)
# 每一行取出：仓库、@ 后面写的东西、注释里的版本（没钉过的没有注释）
PAIRS=$(sed -nE 's/^[[:space:]]*-?[[:space:]]*uses:[[:space:]]*([A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+)@([^[:space:]#]+)([[:space:]]*#[[:space:]]*([^[:space:]]+))?.*$/\1 \2 \4/p' $FILES | sort -u)

echo "$PAIRS" | while read -r repo ref tag; do
    [ -n "$repo" ] || continue
    tag=${tag:-$ref}
    refs=$(git ls-remote --tags --heads "https://github.com/$repo") || { echo "查不到 $repo，检查网络" >&2; exit 1; }
    # 带注释的标签要取它指向的提交（^{} 那一行），轻量标签和分支直接就是提交
    sha=$(printf '%s\n' "$refs" | awk -v t="refs/tags/$tag^{}" '$2==t {print $1; exit}')
    [ -n "$sha" ] || sha=$(printf '%s\n' "$refs" | awk -v t="refs/tags/$tag" '$2==t {print $1; exit}')
    [ -n "$sha" ] || sha=$(printf '%s\n' "$refs" | awk -v t="refs/heads/$tag" '$2==t {print $1; exit}')
    case "$sha" in
        ????????????????????????????????????????) ;;
        *) echo "$repo 没有叫 $tag 的版本" >&2; exit 1 ;;
    esac
    sed -i -E "s|(uses:[[:space:]]*)$repo@$ref([[:space:]]*#.*)?\$|\\1$repo@$sha # $tag|" $FILES
    echo "$repo@$tag → $sha"
done
echo "完成。git diff .github 可以看到改了哪些行。"
