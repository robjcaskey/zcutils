terraform {
  required_version = ">= 1.7.0, < 2.0.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
  # Configure a protected state path with terraform init -backend-config.
  backend "local" {}
}

provider "aws" {
  region = "us-east-1"
  default_tags {
    tags = { Project = "diskpipe-docs", ManagedBy = "terraform" }
  }
}

variable "zone_id" {
  description = "Existing Route53 hosted zone; this module does not own the zone or registration."
  type        = string
  default     = "Z02482853NUNWHLIEN98Q"
}

resource "aws_route53_record" "caa" {
  zone_id = var.zone_id
  name    = "diskpipe.com"
  type    = "CAA"
  ttl     = 300
  records = ["0 issue \"amazon.com\""]
}

resource "aws_acm_certificate" "docs" {
  # A www CNAME to github.io inherits GitHub's CAA policy, which excludes ACM.
  # Point www at our apex and authorize Amazon before requesting either name.
  depends_on                = [aws_route53_record.www, aws_route53_record.caa]
  domain_name               = "diskpipe.com"
  subject_alternative_names = ["www.diskpipe.com"]
  validation_method         = "DNS"
  key_algorithm             = "RSA_2048"
  # The non-exportable public certificate is free with CloudFront.
  options {
    certificate_transparency_logging_preference = "ENABLED"
    export                                      = "DISABLED"
  }
  lifecycle { create_before_destroy = true }
}

resource "aws_route53_record" "validation" {
  # Static keys also permit importing the existing public DNS before issuance.
  for_each = toset(["diskpipe.com", "www.diskpipe.com"])
  zone_id  = var.zone_id
  name = one([
    for option in aws_acm_certificate.docs.domain_validation_options :
    option.resource_record_name if option.domain_name == each.key
  ])
  type = "CNAME"
  ttl  = 300
  records = [one([
    for option in aws_acm_certificate.docs.domain_validation_options :
    option.resource_record_value if option.domain_name == each.key
  ])]
}

resource "aws_acm_certificate_validation" "docs" {
  certificate_arn         = aws_acm_certificate.docs.arn
  validation_record_fqdns = [for record in aws_route53_record.validation : record.fqdn]
}

resource "aws_cloudfront_distribution" "docs" {
  enabled             = true
  is_ipv6_enabled     = true
  aliases             = ["diskpipe.com", "www.diskpipe.com"]
  comment             = "Diskpipe documentation; GitHub Pages origin; no dedicated servers"
  default_root_object = "index.html"
  price_class         = "PriceClass_100"
  wait_for_deployment = true

  origin {
    domain_name = "robjcaskey.github.io"
    origin_path = "/diskpipe-docs"
    origin_id   = "github-pages"
    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "https-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    target_origin_id       = "github-pages"
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD", "OPTIONS"]
    cached_methods         = ["GET", "HEAD"]
    compress               = true
    min_ttl                = 0
    default_ttl            = 60
    max_ttl                = 300
    forwarded_values {
      query_string = false
      cookies { forward = "none" }
    }
  }

  # Real missing documents remain 404s rather than serving an application shell.
  custom_error_response {
    error_code            = 404
    error_caching_min_ttl = 0
  }

  restrictions {
    geo_restriction { restriction_type = "none" }
  }

  viewer_certificate {
    acm_certificate_arn      = aws_acm_certificate_validation.docs.certificate_arn
    ssl_support_method       = "sni-only"
    minimum_protocol_version = "TLSv1.2_2021"
  }
}

resource "aws_route53_record" "apex" {
  for_each = toset(["A", "AAAA"])
  zone_id  = var.zone_id
  name     = "diskpipe.com"
  type     = each.value
  alias {
    name                   = aws_cloudfront_distribution.docs.domain_name
    zone_id                = aws_cloudfront_distribution.docs.hosted_zone_id
    evaluate_target_health = false
  }
}

resource "aws_route53_record" "www" {
  zone_id = var.zone_id
  name    = "www.diskpipe.com"
  type    = "CNAME"
  ttl     = 300
  records = ["diskpipe.com"]
}

output "distribution_id" { value = aws_cloudfront_distribution.docs.id }
output "distribution_domain" { value = aws_cloudfront_distribution.docs.domain_name }
output "certificate_arn" { value = aws_acm_certificate.docs.arn }
output "urls" { value = ["https://diskpipe.com/", "https://www.diskpipe.com/"] }
