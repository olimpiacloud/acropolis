package main

import (
	"net/http"
	"os"
	"runtime"

	"github.com/gin-gonic/gin"
)

type item struct {
	Name string `json:"name" binding:"required"`
	Qty  int    `json:"qty" binding:"required,gt=0"`
}

func main() {
	gin.SetMode(gin.ReleaseMode)
	r := gin.New()
	r.Use(gin.Recovery())
	items := []item{}
	r.GET("/", func(c *gin.Context) {
		c.JSON(http.StatusOK, gin.H{"ok": true, "service": "go-api", "go": runtime.Version()})
	})
	r.GET("/items", func(c *gin.Context) { c.JSON(http.StatusOK, items) })
	r.POST("/items", func(c *gin.Context) {
		var it item
		if err := c.ShouldBindJSON(&it); err != nil {
			c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
			return
		}
		items = append(items, it)
		c.JSON(http.StatusCreated, it)
	})
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	r.Run(":" + port)
}
