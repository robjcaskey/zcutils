# These empty network resources have no hourly charge. Public IPv4 addresses are
# allocated by RunInstances at job runtime, never reserved by Terraform.
locals {
  network_vpc_id = var.create_public_network ? aws_vpc.runners[0].id : null
}

data "aws_availability_zones" "available" {
  count = var.create_public_network ? 1 : 0
  state = "available"
  filter {
    name   = "opt-in-status"
    values = ["opt-in-not-required"]
  }
}

resource "aws_vpc" "runners" {
  count                = var.create_public_network ? 1 : 0
  cidr_block           = "10.84.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags                 = { Name = var.name_prefix, RunnerPool = var.name_prefix }
}

resource "aws_internet_gateway" "runners" {
  count  = var.create_public_network ? 1 : 0
  vpc_id = local.network_vpc_id
  tags   = { Name = var.name_prefix }
}

resource "aws_subnet" "runners" {
  count                   = var.create_public_network ? 1 : 0
  vpc_id                  = local.network_vpc_id
  cidr_block              = var.public_subnet_cidr
  availability_zone       = sort(data.aws_availability_zones.available[0].names)[0]
  map_public_ip_on_launch = true
  tags                    = { Name = var.name_prefix, RunnerPool = var.name_prefix }
}

resource "aws_route_table" "runners" {
  count  = var.create_public_network ? 1 : 0
  vpc_id = local.network_vpc_id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.runners[0].id
  }
  tags = { Name = var.name_prefix }
}

resource "aws_route_table_association" "runners" {
  count          = var.create_public_network ? 1 : 0
  subnet_id      = aws_subnet.runners[0].id
  route_table_id = aws_route_table.runners[0].id
}

resource "aws_security_group" "runners" {
  count       = var.create_public_network ? 1 : 0
  name        = var.name_prefix
  description = "Ephemeral GitHub runners initiate outbound connections; no inbound access"
  vpc_id      = local.network_vpc_id
  tags        = { Name = var.name_prefix, RunnerPool = var.name_prefix }
}

resource "aws_vpc_security_group_egress_rule" "runners" {
  count             = var.create_public_network ? 1 : 0
  security_group_id = aws_security_group.runners[0].id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "GitHub, RHEL repositories, and build dependencies"
}
