package main

import (
	"bytes"
	"os"

	"github.com/vicanso/elton"
	"github.com/vicanso/elton/middleware"
)

func main() {
	staticPath := os.Getenv("STATIC")
	e := elton.New()

	sf := new(middleware.FS)
	e.GET("/ping", func(c *elton.Context) error {
		c.BodyBuffer = bytes.NewBufferString("pong")
		return nil
	})
	// static file route
	e.GET("/*", middleware.NewStaticServe(sf, middleware.StaticServeConfig{
		Path: staticPath,
		// 客户端缓存一年
		MaxAge: 365 * 24 * 3600,
		// 缓存服务器缓存一个小时
		SMaxAge:             60 * 60,
		DenyQueryString:     true,
		DisableLastModified: true,
		// 如果使用packr，它不支持Stat，因此需要用强ETag
		EnableStrongETag: true,
	}))

	err := e.ListenAndServe(":3000")
	if err != nil {
		panic(err)
	}
}
