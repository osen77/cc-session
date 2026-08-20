#!/bin/bash
# Release script for ccs (claude-code-sync)
# Supports both interactive and non-interactive (CI/Claude) usage.
#
# Interactive:  ./scripts/release.sh
# Non-interactive: ./scripts/release.sh patch|minor|major|push [-m "commit message"] [-y]
#
# Options:
#   -m MSG   Commit message for uncommitted changes (default: auto-generated)
#   -y       Skip confirmation prompts

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Get project directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# Get current version
CURRENT_VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"

# Calculate next versions
NEXT_PATCH="${MAJOR}.${MINOR}.$((PATCH + 1))"
NEXT_MINOR="${MAJOR}.$((MINOR + 1)).0"
NEXT_MAJOR="$((MAJOR + 1)).0.0"

# Parse arguments
ACTION=""
COMMIT_MSG=""
AUTO_YES=false

# First positional arg is the action
if [ $# -ge 1 ] && [[ "$1" =~ ^(patch|minor|major|push)$ ]]; then
    ACTION="$1"
    shift
fi

# Parse flags
while [[ $# -gt 0 ]]; do
    case "$1" in
        -m)
            COMMIT_MSG="$2"
            shift 2
            ;;
        -y)
            AUTO_YES=true
            shift
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            echo "Usage: $0 [patch|minor|major|push] [-m \"message\"] [-y]"
            exit 1
            ;;
    esac
done

# Interactive mode if no action specified
if [ -z "$ACTION" ]; then
    echo ""
    echo -e "${BOLD}╔════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}║          ccs Release Tool              ║${NC}"
    echo -e "${BOLD}╚════════════════════════════════════════╝${NC}"
    echo ""
    echo -e "Current version: ${GREEN}v${CURRENT_VERSION}${NC}"
    echo ""
    echo -e "${BOLD}Select an action:${NC}"
    echo ""
    echo -e "  ${CYAN}1)${NC} Push only         - Push commits to remote (no version change)"
    echo -e "  ${CYAN}2)${NC} Release patch     - ${YELLOW}v${CURRENT_VERSION}${NC} → ${GREEN}v${NEXT_PATCH}${NC}  (bug fixes)"
    echo -e "  ${CYAN}3)${NC} Release minor     - ${YELLOW}v${CURRENT_VERSION}${NC} → ${GREEN}v${NEXT_MINOR}${NC}  (new features)"
    echo -e "  ${CYAN}4)${NC} Release major     - ${YELLOW}v${CURRENT_VERSION}${NC} → ${GREEN}v${NEXT_MAJOR}${NC}  (breaking changes)"
    echo -e "  ${CYAN}q)${NC} Quit"
    echo ""

    read -p "Your choice [1-4/q]: " choice

    case "$choice" in
        1) ACTION="push" ;;
        2) ACTION="patch" ;;
        3) ACTION="minor" ;;
        4) ACTION="major" ;;
        q|Q) echo -e "${YELLOW}Cancelled.${NC}"; exit 0 ;;
        *) echo -e "${RED}Invalid choice.${NC}"; exit 1 ;;
    esac
    echo ""
fi

# Resolve new version
case "$ACTION" in
    patch) NEW_VERSION="$NEXT_PATCH" ;;
    minor) NEW_VERSION="$NEXT_MINOR" ;;
    major) NEW_VERSION="$NEXT_MAJOR" ;;
    push)  NEW_VERSION="" ;;
esac

# Check for uncommitted changes
HAS_CHANGES=false
if ! git diff --quiet || ! git diff --staged --quiet || [ -n "$(git ls-files --others --exclude-standard)" ]; then
    HAS_CHANGES=true
    echo -e "${YELLOW}Uncommitted changes detected:${NC}"
    git status --short
    echo ""
fi

if [ "$ACTION" = "push" ]; then
    # Push only
    if [ "$HAS_CHANGES" = true ]; then
        if [ "$AUTO_YES" = true ]; then
            # Auto-commit with provided or default message
            [ -z "$COMMIT_MSG" ] && COMMIT_MSG="chore: update"
            git add -A
            git commit -m "$COMMIT_MSG"
        else
            read -p "Commit all changes before push? [Y/n]: " -n 1 -r
            echo ""
            if [[ ! $REPLY =~ ^[Nn]$ ]]; then
                if [ -z "$COMMIT_MSG" ]; then
                    read -p "Commit message: " COMMIT_MSG
                    [ -z "$COMMIT_MSG" ] && COMMIT_MSG="chore: update"
                fi
                git add -A
                git commit -m "$COMMIT_MSG"
            fi
        fi
    fi
    echo -e "${CYAN}Pushing to remote...${NC}"
    git push origin HEAD
    echo ""
    echo -e "${GREEN}✓ Push complete!${NC}"
else
    # Release with version bump
    TAG_NAME="v${NEW_VERSION}"

    # Check if tag exists
    if git rev-parse "$TAG_NAME" >/dev/null 2>&1; then
        echo -e "${RED}Error: Tag ${TAG_NAME} already exists!${NC}"
        exit 1
    fi

    echo -e "${CYAN}Releasing ${YELLOW}v${CURRENT_VERSION}${NC} → ${GREEN}v${NEW_VERSION}${NC}"

    # Confirm unless auto-yes
    if [ "$AUTO_YES" != true ]; then
        read -p "Continue? [Y/n]: " -n 1 -r
        echo ""
        if [[ $REPLY =~ ^[Nn]$ ]]; then
            echo -e "${YELLOW}Cancelled.${NC}"
            exit 0
        fi
    fi

    # Stage all changes first if any
    if [ "$HAS_CHANGES" = true ]; then
        echo -e "${CYAN}Staging all changes...${NC}"
        git add -A
    fi

    echo -e "${CYAN}Updating version to ${NEW_VERSION}...${NC}"
    sed -i '' "s/^version = \"${CURRENT_VERSION}\"/version = \"${NEW_VERSION}\"/" Cargo.toml
    # Sync Cargo.lock: the release workflow builds with --locked, and a tag whose
    # lockfile still records the old version fails every platform build (v0.5.6).
    cargo check --quiet
    if ! grep -A1 '^name = "claude-code-sync"$' Cargo.lock | grep -q "version = \"${NEW_VERSION}\""; then
        echo -e "${RED}Error: Cargo.lock did not pick up ${NEW_VERSION}; aborting before tag.${NC}"
        exit 1
    fi
    git add Cargo.toml Cargo.lock

    echo -e "${CYAN}Committing...${NC}"
    git commit -m "chore: bump version to ${NEW_VERSION}"

    echo -e "${CYAN}Creating tag ${TAG_NAME}...${NC}"
    git tag -a "$TAG_NAME" -m "Release ${TAG_NAME}"

    echo -e "${CYAN}Pushing...${NC}"
    git push origin HEAD
    git push origin "$TAG_NAME"

    echo ""
    echo -e "${GREEN}✓ Release ${TAG_NAME} complete!${NC}"
    echo ""
    echo -e "GitHub Actions: ${CYAN}https://github.com/osen77/cc-session/actions${NC}"
fi
