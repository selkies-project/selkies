# A Coder module: Selkies in any workspace with a desktop installed, as a
# workspace app Coder authenticates and proxies. Its variables match the
# KasmVNC module's, so a template swaps one for the other or runs both.
terraform {
  required_version = ">= 1.0"
  required_providers {
    coder = {
      source  = "coder/coder"
      version = ">= 2.5"
    }
  }
}

variable "agent_id" {
  type        = string
  description = "The ID of a Coder agent."
}

variable "port" {
  type        = number
  description = "The port Selkies listens on, on the workspace's loopback addresses."
  default     = 8080
}

variable "desktop_environment" {
  type        = string
  description = "A session by file, desktop, or program name (xfce, kde, lxqt, gnome, mate) or a command; empty takes the workspace's default desktop."
  default     = ""
}

variable "wayland" {
  type        = bool
  description = "Stream through Selkies' Wayland backend instead of an Xvfb."
  default     = false
}

variable "selkies_version" {
  type        = string
  description = "The Selkies release to install where the workspace has none; empty takes the latest."
  default     = ""
}

variable "order" {
  type        = number
  description = "The order determines the position of app in the UI presentation. The lowest order is shown first and apps with equal order are sorted by name (ascending order)."
  default     = null
}

variable "group" {
  type        = string
  description = "The name of a group that this app belongs to."
  default     = null
}

variable "subdomain" {
  type        = bool
  default     = true
  description = "Is subdomain sharing enabled in your cluster?"
}

variable "share" {
  type    = string
  default = "owner"
  validation {
    condition     = var.share == "owner" || var.share == "authenticated" || var.share == "public"
    error_message = "Incorrect value. Please set either 'owner', 'authenticated', or 'public'."
  }
}

locals {
  icon = "https://raw.githubusercontent.com/selkies-project/selkies/main/docs/assets/logo/selkies.svg"
}

resource "coder_script" "selkies" {
  agent_id     = var.agent_id
  display_name = "Selkies"
  icon         = local.icon
  run_on_start = true
  script = templatefile("${path.module}/run.sh.tftpl", {
    PORT                = var.port
    DESKTOP_ENVIRONMENT = replace(var.desktop_environment, "'", "'\\''")
    WAYLAND             = tostring(var.wayland)
    SELKIES_VERSION     = var.selkies_version
  })
}

resource "coder_app" "selkies" {
  agent_id     = var.agent_id
  slug         = "selkies"
  display_name = "Selkies"
  url          = "http://localhost:${var.port}"
  icon         = local.icon
  subdomain    = var.subdomain
  share        = var.share
  order        = var.order
  group        = var.group

  healthcheck {
    url       = "http://localhost:${var.port}/api/health"
    interval  = 5
    threshold = 6
  }
}
